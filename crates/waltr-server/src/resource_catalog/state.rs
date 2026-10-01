//! Server-owned unified resource catalog lifecycle and materialization.

use waltr_agent::{AgentDefinitionBundle, AgentDefinitionCatalog, ResolvedAgent};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fmt, fs, io,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use crate::catalog::CatalogMaterializer;
use waltr::grpc::waltr::config::v1 as config_proto;
use waltr::grpc::waltr::resource::v1 as resource_proto;

pub use super::error::AgentDefinitionLoadError;
use super::{
    agent::compile_catalog,
    materialize::{
        invalid_value, materialize_defaults, materialize_mcp_server, materialize_provider,
        materialized_snapshot_etag, proto_kind, proto_resource, yaml_json,
    },
    routine::{MAX_ROUTINE_COUNT, MaterializedRoutine, materialize_routine},
    store::{
        PersistedSlotPostcondition, atomic_write, check_expected_etag, discover_persisted,
        discover_persisted_for, load_external_sources, resource_disk_path, slot_etag, source_etag,
        tree_fingerprint, validate_resource_path, validate_resource_source,
    },
};

const DEFAULT_AGENT: &str = "coding";
const ENV_DEFINITION_PATHS: &str = "WALTR_AGENT_DEFINITION_PATHS";
const ENV_DEFAULT_AGENT: &str = "WALTR_DEFAULT_AGENT";

const BUNDLED_RESOURCES: [(&str, &str); 6] = [
    (
        "agents/coding.yaml",
        include_str!("../../resources/agents/coding.yaml"),
    ),
    (
        "agents/configuration.yaml",
        include_str!("../../resources/agents/configuration.yaml"),
    ),
    (
        "agents/default.yaml",
        include_str!("../../resources/agents/default.yaml"),
    ),
    (
        "features/coding.yaml",
        include_str!("../../resources/features/coding.yaml"),
    ),
    (
        "permissions/coding.yaml",
        include_str!("../../resources/permissions/coding.yaml"),
    ),
    (
        "permissions/default.yaml",
        include_str!("../../resources/permissions/default.yaml"),
    ),
];

const UNIFIED_BUNDLED_RESOURCES: [(&str, &str); 15] = [
    BUNDLED_RESOURCES[0],
    BUNDLED_RESOURCES[1],
    BUNDLED_RESOURCES[2],
    BUNDLED_RESOURCES[3],
    BUNDLED_RESOURCES[4],
    BUNDLED_RESOURCES[5],
    (
        "providers/anthropic.yaml",
        include_str!("../../resources/providers/anthropic.yaml"),
    ),
    (
        "providers/claude-code.yaml",
        include_str!("../../resources/providers/claude-code.yaml"),
    ),
    (
        "providers/openai.yaml",
        include_str!("../../resources/providers/openai.yaml"),
    ),
    (
        "providers/codex.yaml",
        include_str!("../../resources/providers/codex.yaml"),
    ),
    (
        "mcp-servers/waltr.yaml",
        include_str!("../../resources/mcp-servers/waltr.yaml"),
    ),
    (
        "defaults/default.yaml",
        include_str!("../../resources/defaults/default.yaml"),
    ),
    (
        "routines/title.yaml",
        include_str!("../../resources/routines/title.yaml"),
    ),
    (
        "routines/compact-and-continue.yaml",
        include_str!("../../resources/routines/compact-and-continue.yaml"),
    ),
    (
        "routines/fork.yaml",
        include_str!("../../resources/routines/fork.yaml"),
    ),
];

/// Canonical resource kinds in the unified catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum ResourceKind {
    /// Runnable agent definition.
    Agent,
    /// Reusable feature definition.
    FeatureSet,
    /// Reusable permission definition.
    PermissionSet,
    /// Inference provider definition.
    Provider,
    /// Outbound MCP server definition.
    McpServer,
    /// Singleton inference defaults.
    Defaults,
    /// Server-owned executable routine definition.
    Routine,
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Agent => "Agent",
            Self::FeatureSet => "FeatureSet",
            Self::PermissionSet => "PermissionSet",
            Self::Provider => "Provider",
            Self::McpServer => "McpServer",
            Self::Defaults => "Defaults",
            Self::Routine => "Routine",
        })
    }
}

/// Load the bundled and environment-configured definitions and resolve the default agent.
///
/// # Errors
///
/// Returns an error when environment values, bundled definitions, or configured external sources
/// are invalid or cannot be loaded.
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
    validate_default_agent(&value)?;
    Ok(value)
}

/// Load bundled definitions followed by the configured external sources.
///
/// # Errors
///
/// Returns an error when the default name or any bundled or external definition is invalid.
pub fn load_default_agent(
    paths: &[PathBuf],
    default_agent: &str,
) -> Result<LoadedAgentDefinition, AgentDefinitionLoadError> {
    validate_default_agent(default_agent)?;
    let external_sources = load_external_sources(paths)?;
    let snapshot = build_snapshot(
        bundled_resources(),
        &BTreeMap::new(),
        &external_sources,
        default_agent,
        None,
    )?;
    Ok(snapshot.loaded())
}

fn validate_default_agent(default_agent: &str) -> Result<(), AgentDefinitionLoadError> {
    if default_agent.trim().is_empty() {
        Err(AgentDefinitionLoadError::EmptyDefaultAgent)
    } else {
        Ok(())
    }
}

/// Selected runtime policy and the exact ordered sources that produced it.
#[derive(Debug, Clone)]
pub struct LoadedAgentDefinition {
    /// Resolved policy installed in the runtime factory.
    pub resolved: ResolvedAgent,
    /// Self-contained source bundle persisted with newly created sessions.
    pub bundle: AgentDefinitionBundle,
    /// Every selectable, compiled-valid startup agent.
    pub catalog: AgentDefinitionCatalog,
}

/// Immutable, atomically published agent-definition state.
#[derive(Debug, Clone)]
pub struct AgentDefinitionSnapshot {
    catalog: AgentDefinitionCatalog,
    default_agent: String,
    sources: Vec<String>,
    entries: BTreeMap<String, ActiveResource>,
    persisted_fingerprint: String,
    configuration: config_proto::Configuration,
    routines: BTreeMap<String, MaterializedRoutine>,
    etag: String,
}

impl AgentDefinitionSnapshot {
    /// Return the active catalog.
    #[must_use]
    pub const fn catalog(&self) -> &AgentDefinitionCatalog {
        &self.catalog
    }

    /// Return the active default agent name.
    #[must_use]
    pub fn default_agent_name(&self) -> &str {
        &self.default_agent
    }

    /// Return the exact ordered source strings used by the active catalog.
    #[must_use]
    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    /// Return the active persisted-tree fingerprint.
    #[must_use]
    pub fn persisted_fingerprint(&self) -> &str {
        &self.persisted_fingerprint
    }

    /// Return the effective provider, MCP, and inference-default projection.
    #[must_use]
    pub const fn configuration(&self) -> &config_proto::Configuration {
        &self.configuration
    }

    /// Look up one materialized routine by canonical name.
    #[must_use]
    pub fn routine(&self, name: &str) -> Option<&MaterializedRoutine> {
        self.routines.get(name)
    }

    /// Iterate over materialized routines in canonical name order.
    #[must_use]
    pub fn routines(&self) -> impl ExactSizeIterator<Item = &MaterializedRoutine> {
        self.routines.values()
    }

    /// Return the materialized aggregate snapshot `ETag`.
    #[must_use]
    pub fn etag(&self) -> &str {
        &self.etag
    }

    fn loaded(&self) -> LoadedAgentDefinition {
        let resolved = self
            .catalog
            .resolved(self.catalog.default_agent_name())
            .expect("snapshot construction validates the default agent")
            .clone();
        LoadedAgentDefinition {
            resolved,
            bundle: AgentDefinitionBundle::new(&self.default_agent, self.sources.clone()),
            catalog: self.catalog.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ActiveResource {
    pub(super) source: String,
    pub(super) etag: String,
    pub(super) origin: ResourceOrigin,
    pub(super) kind: ResourceKind,
    pub(super) name: String,
    pub(super) effective_value: serde_json::Value,
}

/// Origin of an effective or active definition resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceOrigin {
    /// Compiled into the server.
    Bundled,
    /// Selected from `.waltr/resources`.
    Filesystem,
}

#[derive(Debug, Clone)]
pub struct PublishedState {
    snapshot: Arc<AgentDefinitionSnapshot>,
    reload_error: Option<String>,
    persisted: Arc<BTreeMap<String, String>>,
}

#[derive(Debug)]
pub(super) struct AgentDefinitionStateInner {
    pub(super) resources_root: PathBuf,
    external_sources: Vec<String>,
    default_agent: String,
    published: RwLock<PublishedState>,
    maintenance: tokio::sync::Mutex<()>,
    bundled: BTreeMap<String, String>,
    materializer: Option<CatalogMaterializer>,
    updates: tokio::sync::broadcast::Sender<PublishedState>,
}

/// Cheap cloneable handle to the current last-known-good definition catalog.
#[derive(Debug, Clone)]
pub struct AgentDefinitionState {
    pub(super) inner: Arc<AgentDefinitionStateInner>,
}

impl AgentDefinitionState {
    /// Load and validate persisted resources under `<root>/.waltr/resources`.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted or external resources cannot be read, validated, or
    /// resolved into a complete catalog.
    pub fn load(
        root: impl Into<PathBuf>,
        external_paths: &[PathBuf],
        default_agent: impl Into<String>,
    ) -> Result<Self, AgentDefinitionLoadError> {
        let root = root.into();
        let default_agent = default_agent.into();
        validate_default_agent(&default_agent)?;
        let external_sources = load_external_sources(external_paths)?;
        let resources_root = root.join(".waltr/resources");
        let persisted = discover_persisted(&resources_root)?;
        let bundled = bundled_resources();
        let snapshot = build_snapshot(
            bundled.clone(),
            &persisted,
            &external_sources,
            &default_agent,
            None,
        )?;
        let (updates, _) = tokio::sync::broadcast::channel(32);
        Ok(Self {
            inner: Arc::new(AgentDefinitionStateInner {
                resources_root,
                external_sources,
                default_agent,
                published: RwLock::new(PublishedState {
                    snapshot: Arc::new(snapshot),
                    reload_error: None,
                    persisted: Arc::new(persisted),
                }),
                maintenance: tokio::sync::Mutex::new(()),
                bundled,
                materializer: None,
                updates,
            }),
        })
    }

    /// Load the complete seven-kind resource catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when bundled or persisted resources cannot be parsed, materialized, or
    /// resolved into one valid snapshot.
    pub fn load_unified(
        root: impl Into<PathBuf>,
        materializer: CatalogMaterializer,
    ) -> Result<Self, AgentDefinitionLoadError> {
        let root = root.into();
        let default_agent =
            env::var(ENV_DEFAULT_AGENT).unwrap_or_else(|_| DEFAULT_AGENT.to_owned());
        validate_default_agent(&default_agent)?;
        let external_sources = Vec::new();
        let resources_root = root.join(".waltr/resources");
        let bundled = unified_bundled_resources();
        let persisted = discover_persisted_for(&resources_root, true)?;
        let snapshot = build_snapshot(
            bundled.clone(),
            &persisted,
            &external_sources,
            &default_agent,
            Some(&materializer),
        )?;
        let (updates, _) = tokio::sync::broadcast::channel(32);
        Ok(Self {
            inner: Arc::new(AgentDefinitionStateInner {
                resources_root,
                external_sources,
                default_agent,
                published: RwLock::new(PublishedState {
                    snapshot: Arc::new(snapshot),
                    reload_error: None,
                    persisted: Arc::new(persisted),
                }),
                maintenance: tokio::sync::Mutex::new(()),
                bundled,
                materializer: Some(materializer),
                updates,
            }),
        })
    }

    /// Load the unified catalog with an explicit default agent without consulting process state.
    ///
    /// # Errors
    ///
    /// Returns an error when bundled or persisted resources cannot be materialized.
    pub(crate) fn load_unified_explicit(
        root: impl Into<PathBuf>,
        materializer: CatalogMaterializer,
        default_agent: impl Into<String>,
    ) -> Result<Self, AgentDefinitionLoadError> {
        let default_agent = default_agent.into();
        validate_default_agent(&default_agent)?;
        let resources_root = root.into().join(".waltr/resources");
        let persisted = discover_persisted_for(&resources_root, true)?;
        let bundled = unified_bundled_resources();
        let snapshot = build_snapshot(
            bundled.clone(),
            &persisted,
            &[],
            &default_agent,
            Some(&materializer),
        )?;
        let (updates, _) = tokio::sync::broadcast::channel(32);
        Ok(Self {
            inner: Arc::new(AgentDefinitionStateInner {
                resources_root,
                external_sources: Vec::new(),
                default_agent,
                published: RwLock::new(PublishedState {
                    snapshot: Arc::new(snapshot),
                    reload_error: None,
                    persisted: Arc::new(persisted),
                }),
                maintenance: tokio::sync::Mutex::new(()),
                bundled,
                materializer: Some(materializer),
                updates,
            }),
        })
    }

    /// Load using the process definition-path and default-agent environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error when environment values or discovered definitions are invalid.
    pub fn load_from_env(root: impl Into<PathBuf>) -> Result<Self, AgentDefinitionLoadError> {
        let paths = env::var_os(ENV_DEFINITION_PATHS)
            .map(|value| env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();
        let default_agent = env::var_os(ENV_DEFAULT_AGENT)
            .map(os_string_to_default_name)
            .transpose()?
            .unwrap_or_else(|| DEFAULT_AGENT.to_owned());
        Self::load(root, &paths, default_agent)
    }

    /// Return the current immutable last-known-good snapshot.
    ///
    /// # Panics
    ///
    /// Panics if another thread poisoned the internal publication lock.
    #[must_use]
    pub fn snapshot(&self) -> Arc<AgentDefinitionSnapshot> {
        self.publication().snapshot
    }

    /// Return the current reload error, if a direct edit was rejected.
    ///
    /// # Panics
    ///
    /// Panics if another thread poisoned the internal publication lock.
    #[must_use]
    pub fn reload_error(&self) -> Option<String> {
        self.publication().reload_error
    }

    /// Subscribe to successful publications and rejected reloads.
    #[must_use]
    pub(crate) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PublishedState> {
        self.inner.updates.subscribe()
    }

    pub(crate) fn publication(&self) -> PublishedState {
        self.inner
            .published
            .read()
            .expect("agent-definition state lock poisoned")
            .clone()
    }

    /// Project the current immutable snapshot into the public resource contract.
    #[must_use]
    pub fn resource_catalog(&self) -> resource_proto::ResourceCatalog {
        self.publication().resource_catalog()
    }

    fn resource_catalog_from(publication: &PublishedState) -> resource_proto::ResourceCatalog {
        publication.resource_catalog()
    }

    /// Project one immutable publication into the public resource contract.
    pub(crate) fn published_resource_catalog(
        publication: &PublishedState,
    ) -> resource_proto::ResourceCatalog {
        Self::resource_catalog_from(publication)
    }

    fn update_publication(
        &self,
        snapshot: Option<AgentDefinitionSnapshot>,
        persisted: Option<BTreeMap<String, String>>,
        reload_error: Option<String>,
    ) {
        let publication = {
            let mut published = self
                .inner
                .published
                .write()
                .expect("agent-definition state lock poisoned");
            if let Some(snapshot) = snapshot {
                published.snapshot = Arc::new(snapshot);
            }
            if let Some(persisted) = persisted {
                published.persisted = Arc::new(persisted);
            }
            published.reload_error = reload_error;
            published.clone()
        };
        drop(self.inner.updates.send(publication));
    }

    fn projected_catalog(publication: &PublishedState) -> resource_proto::ResourceCatalog {
        let snapshot = &publication.snapshot;
        resource_proto::ResourceCatalog {
            resources: snapshot
                .entries
                .iter()
                .map(|(path, entry)| proto_resource(path, entry, publication.persisted.get(path)))
                .collect(),
            effective_configuration: Some(snapshot.configuration.clone()),
            etag: snapshot.etag.clone(),
            reload_error: publication.reload_error.clone().unwrap_or_default(),
        }
    }

    /// Project current persisted slots and active values into the inspection contract.
    ///
    /// # Errors
    ///
    /// Returns an error when the optional path is not canonical or the persisted tree cannot be
    /// inspected.
    pub fn resource_inspections(
        &self,
        path: Option<&str>,
    ) -> Result<Vec<resource_proto::ResourceInspection>, AgentDefinitionLoadError> {
        let publication = self.publication();
        self.resource_inspections_from(&publication, path)
    }

    pub(crate) fn resource_inspection_snapshot(
        &self,
        path: Option<&str>,
    ) -> Result<(Vec<resource_proto::ResourceInspection>, String, String), AgentDefinitionLoadError>
    {
        let publication = self.publication();
        let resources = self.resource_inspections_from(&publication, path)?;
        Ok((
            resources,
            publication.snapshot.etag().to_owned(),
            publication.reload_error.clone().unwrap_or_default(),
        ))
    }

    fn resource_inspections_from(
        &self,
        publication: &PublishedState,
        path: Option<&str>,
    ) -> Result<Vec<resource_proto::ResourceInspection>, AgentDefinitionLoadError> {
        let persisted = &publication.persisted;
        let snapshot = &publication.snapshot;
        let paths = if let Some(path) = path {
            validate_resource_path(path)?;
            vec![path.to_owned()]
        } else {
            let mut paths = self.inner.bundled.keys().cloned().collect::<BTreeSet<_>>();
            paths.extend(persisted.keys().cloned());
            paths.into_iter().collect()
        };
        let reload_error = publication.reload_error.clone().unwrap_or_default();
        paths
            .into_iter()
            .map(|path| {
                let (kind, name) = validate_resource_path(&path)?;
                let persisted_source = persisted.get(&path);
                let active = snapshot.entries.get(&path);
                Ok(resource_proto::ResourceInspection {
                    path: path.clone(),
                    kind: proto_kind(kind) as i32,
                    name,
                    persisted: persisted_source.is_some(),
                    persisted_etag: slot_etag(persisted_source.map(String::as_bytes)),
                    persisted_source: persisted_source.cloned(),
                    active: active.map(|entry| proto_resource(&path, entry, persisted_source)),
                    active_source: active.map(|entry| entry.source.clone()),
                    reload_error: reload_error.clone(),
                })
            })
            .collect()
    }

    /// Inspect all current effective resource slots or one canonical path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is invalid or the persisted tree cannot be inspected.
    pub fn inspect(
        &self,
        path: Option<&str>,
    ) -> Result<AgentDefinitionInspection, AgentDefinitionLoadError> {
        let publication = self.publication();
        let persisted = &publication.persisted;
        let active = &publication.snapshot;
        let reload_error = publication.reload_error.clone();
        if let Some(path) = path {
            validate_resource_path(path)?;
            return Ok(AgentDefinitionInspection::Resource(Box::new(
                inspect_resource(path, persisted, active, &self.inner.bundled, reload_error),
            )));
        }

        let mut paths = self.inner.bundled.keys().cloned().collect::<BTreeSet<_>>();
        paths.extend(persisted.keys().cloned());
        let entries = paths
            .into_iter()
            .map(|path| inspect_summary(&path, persisted, active, &self.inner.bundled))
            .collect();
        Ok(AgentDefinitionInspection::Catalog {
            entries,
            reload_error,
        })
    }

    /// Replace one complete persisted resource after stale-write and candidate validation.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, a stale `ETag`, failed validation or persistence, or a
    /// concurrent filesystem change.
    pub async fn replace(
        &self,
        path: &str,
        source: String,
        expected_etag: &str,
    ) -> Result<DefinitionResourceDetail, AgentDefinitionLoadError> {
        self.replace_guarded(path, source, expected_etag, None)
            .await
    }

    /// Replace a slot while also guarding the effective source used for a text edit.
    ///
    /// # Errors
    ///
    /// Returns the ordinary replace errors or a concurrent-change error when the source changed.
    pub(crate) async fn replace_guarded(
        &self,
        path: &str,
        source: String,
        expected_etag: &str,
        expected_source: Option<&str>,
    ) -> Result<DefinitionResourceDetail, AgentDefinitionLoadError> {
        validate_resource_path(path)?;
        if expected_etag.trim().is_empty() {
            return Err(AgentDefinitionLoadError::MissingExpectedEtag);
        }
        validate_resource_source(path, &source)?;
        let _maintenance = self.inner.maintenance.lock().await;
        let mut persisted = self.discover_persisted()?;
        check_expected_etag(path, persisted.get(path), expected_etag)?;
        if let Some(expected) = expected_source {
            let effective = persisted.get(path).or_else(|| self.inner.bundled.get(path));
            if effective.map(String::as_str) != Some(expected) {
                return Err(AgentDefinitionLoadError::ConcurrentFilesystemChange);
            }
        }
        let observed_fingerprint = tree_fingerprint(&persisted);
        persisted.insert(path.to_owned(), source.clone());
        drop(self.build_candidate(&persisted)?);
        self.ensure_tree_unchanged(&observed_fingerprint)?;
        atomic_write(&self.inner.resources_root, path, source.as_bytes())?;
        let candidate = self
            .stabilize_after_mutation(path, PersistedSlotPostcondition::Exact(source.as_str()))?;
        self.publish(candidate);
        self.inspect_detail(path)
    }

    /// Remove one persisted resource after stale-write and fallback validation.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid path, a stale `ETag`, failed fallback validation or
    /// removal, or a concurrent filesystem change.
    pub async fn reset(
        &self,
        path: &str,
        expected_etag: &str,
    ) -> Result<DefinitionResourceDetail, AgentDefinitionLoadError> {
        validate_resource_path(path)?;
        if expected_etag.trim().is_empty() {
            return Err(AgentDefinitionLoadError::MissingExpectedEtag);
        }
        let _maintenance = self.inner.maintenance.lock().await;
        let mut persisted = self.discover_persisted()?;
        check_expected_etag(path, persisted.get(path), expected_etag)?;
        let observed_fingerprint = tree_fingerprint(&persisted);
        persisted.remove(path);
        drop(self.build_candidate(&persisted)?);
        self.ensure_tree_unchanged(&observed_fingerprint)?;
        let target = resource_disk_path(&self.inner.resources_root, path);
        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(AgentDefinitionLoadError::Write {
                    path: PathBuf::from(path),
                    source,
                });
            }
        }
        let candidate = self.stabilize_after_mutation(path, PersistedSlotPostcondition::Absent)?;
        self.publish(candidate);
        self.inspect_detail(path)
    }

    fn build_candidate(
        &self,
        persisted: &BTreeMap<String, String>,
    ) -> Result<AgentDefinitionSnapshot, AgentDefinitionLoadError> {
        build_snapshot(
            self.inner.bundled.clone(),
            persisted,
            &self.inner.external_sources,
            &self.inner.default_agent,
            self.inner.materializer.as_ref(),
        )
    }

    fn discover_persisted(&self) -> Result<BTreeMap<String, String>, AgentDefinitionLoadError> {
        discover_persisted_for(
            &self.inner.resources_root,
            self.inner.materializer.is_some(),
        )
    }

    fn ensure_tree_unchanged(&self, expected: &str) -> Result<(), AgentDefinitionLoadError> {
        let current = self.discover_persisted()?;
        if tree_fingerprint(&current) == expected {
            Ok(())
        } else {
            Err(AgentDefinitionLoadError::ConcurrentFilesystemChange)
        }
    }

    fn stabilize_after_mutation(
        &self,
        path: &str,
        postcondition: PersistedSlotPostcondition<'_>,
    ) -> Result<AgentDefinitionSnapshot, AgentDefinitionLoadError> {
        for _ in 0..8 {
            let persisted = match self.discover_persisted() {
                Ok(persisted) => persisted,
                Err(error) => return Err(self.recorded_error(error)),
            };
            if !postcondition.matches(persisted.get(path)) {
                return Err(
                    self.recorded_error(AgentDefinitionLoadError::ConcurrentFilesystemChange)
                );
            }
            let fingerprint = tree_fingerprint(&persisted);
            let candidate = match self.build_candidate(&persisted) {
                Ok(candidate) => candidate,
                Err(error) => return Err(self.recorded_error(error)),
            };
            let current = match self.discover_persisted() {
                Ok(current) => current,
                Err(error) => return Err(self.recorded_error(error)),
            };
            if tree_fingerprint(&current) != fingerprint {
                continue;
            }
            if !postcondition.matches(current.get(path)) {
                return Err(
                    self.recorded_error(AgentDefinitionLoadError::ConcurrentFilesystemChange)
                );
            }
            return Ok(candidate);
        }
        Err(self.recorded_error(AgentDefinitionLoadError::ConcurrentFilesystemChange))
    }

    fn publish(&self, snapshot: AgentDefinitionSnapshot) {
        let persisted = self.discover_persisted().ok();
        self.update_publication(Some(snapshot), persisted, None);
    }

    fn inspect_detail(
        &self,
        path: &str,
    ) -> Result<DefinitionResourceDetail, AgentDefinitionLoadError> {
        let AgentDefinitionInspection::Resource(detail) = self.inspect(Some(path))? else {
            unreachable!("path inspection always returns one resource")
        };
        Ok(*detail)
    }

    pub(crate) async fn reload_from_disk(&self) {
        let _maintenance = self.inner.maintenance.lock().await;
        for _ in 0..8 {
            let persisted = match self.discover_persisted() {
                Ok(persisted) => persisted,
                Err(error) => {
                    self.record_reload_error(error.to_string());
                    return;
                }
            };
            let fingerprint = tree_fingerprint(&persisted);
            if fingerprint == self.snapshot().persisted_fingerprint {
                self.update_publication(None, Some(persisted), None);
                return;
            }
            let candidate = match self.build_candidate(&persisted) {
                Ok(candidate) => candidate,
                Err(error) => {
                    self.record_reload_error_with_persisted(error.to_string(), persisted);
                    return;
                }
            };
            let current = match self.discover_persisted() {
                Ok(current) => current,
                Err(error) => {
                    self.record_reload_error(error.to_string());
                    return;
                }
            };
            if tree_fingerprint(&current) != fingerprint {
                continue;
            }
            self.publish(candidate);
            return;
        }
        self.record_reload_error(
            "resource tree kept changing during reload; retrying on the next filesystem event"
                .to_owned(),
        );
    }

    fn record_reload_error(&self, error: String) {
        self.update_publication(None, None, Some(error));
    }

    fn record_reload_error_with_persisted(
        &self,
        error: String,
        persisted: BTreeMap<String, String>,
    ) {
        self.update_publication(None, Some(persisted), Some(error));
    }

    fn recorded_error(&self, error: AgentDefinitionLoadError) -> AgentDefinitionLoadError {
        self.record_reload_error(error.to_string());
        error
    }
}

/// JSON-ready inspection result.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum AgentDefinitionInspection {
    /// Deterministic summary of all effective disk slots.
    Catalog {
        /// Entries sorted by canonical path.
        entries: Vec<DefinitionResourceSummary>,
        /// Current rejected-reload error, if any.
        reload_error: Option<String>,
    },
    /// Detailed inspection of one slot.
    Resource(Box<DefinitionResourceDetail>),
}

/// Summary of one current effective slot.
#[derive(Debug, Clone, Serialize)]
pub struct DefinitionResourceSummary {
    /// Canonical relative resource path.
    pub path: String,
    /// Resource kind implied by the categorized path.
    pub kind: String,
    /// Resource name implied by the filename.
    pub name: String,
    /// Current effective origin.
    pub origin: ResourceOrigin,
    /// Whether a persisted file currently occupies the slot.
    pub persisted: bool,
    /// Exact persisted-slot `ETag`, including the deterministic absent `ETag`.
    pub persisted_etag: String,
    /// `ETag` of the current effective disk source.
    pub effective_etag: String,
    /// `ETag` of the last-known-good active source, if active.
    pub active_etag: Option<String>,
    /// Whether this slot is present in the last-known-good active snapshot.
    pub active: bool,
}

/// Detailed current and last-known-good state for one resource slot.
#[derive(Debug, Clone, Serialize)]
pub struct DefinitionResourceDetail {
    /// Canonical relative resource path.
    pub path: String,
    /// Resource kind implied by the categorized path.
    pub kind: String,
    /// Resource name implied by the filename.
    pub name: String,
    /// Current effective origin, if the slot exists.
    pub origin: Option<ResourceOrigin>,
    /// Last-known-good active origin, if active.
    pub active_origin: Option<ResourceOrigin>,
    /// Whether a persisted file currently occupies the slot.
    pub persisted: bool,
    /// Whether a current effective disk source exists.
    pub effective: bool,
    /// Whether this slot is present in the last-known-good active snapshot.
    pub active: bool,
    /// Exact current effective source, if the slot exists.
    pub source: Option<String>,
    /// Exact persisted source, if present.
    pub persisted_source: Option<String>,
    /// Exact last-known-good active source, if active.
    pub active_source: Option<String>,
    /// Exact persisted-slot `ETag`, including the deterministic absent `ETag`.
    pub persisted_etag: String,
    /// `ETag` of the current effective source, if present.
    pub effective_etag: Option<String>,
    /// `ETag` of the last-known-good active source, if active.
    pub active_etag: Option<String>,
    /// Current rejected-reload error, if any.
    pub reload_error: Option<String>,
}

impl PublishedState {
    pub(crate) fn resource_catalog(&self) -> resource_proto::ResourceCatalog {
        AgentDefinitionState::projected_catalog(self)
    }
}

fn inspect_summary(
    path: &str,
    persisted: &BTreeMap<String, String>,
    active: &AgentDefinitionSnapshot,
    bundled: &BTreeMap<String, String>,
) -> DefinitionResourceSummary {
    let (kind, name) = validate_resource_path(path).expect("stored resource paths are canonical");
    let persisted_source = persisted.get(path);
    let effective_source = persisted_source
        .map(String::as_str)
        .or_else(|| bundled.get(path).map(String::as_str));
    DefinitionResourceSummary {
        path: path.to_owned(),
        kind: kind.to_string(),
        name,
        origin: if persisted_source.is_none() {
            ResourceOrigin::Bundled
        } else {
            ResourceOrigin::Filesystem
        },
        persisted: persisted_source.is_some(),
        persisted_etag: slot_etag(persisted_source.map(String::as_bytes)),
        effective_etag: source_etag(
            effective_source
                .expect("summary paths come from bundled or persisted sources")
                .as_bytes(),
        ),
        active_etag: active.entries.get(path).map(|entry| entry.etag.clone()),
        active: active.entries.contains_key(path),
    }
}

fn inspect_resource(
    path: &str,
    persisted: &BTreeMap<String, String>,
    active: &AgentDefinitionSnapshot,
    bundled: &BTreeMap<String, String>,
    reload_error: Option<String>,
) -> DefinitionResourceDetail {
    let (kind, name) = validate_resource_path(path).expect("inspected resource path is canonical");
    let persisted_source = persisted.get(path);
    let effective_source = persisted_source
        .map(String::as_str)
        .or_else(|| bundled.get(path).map(String::as_str));
    let active = active.entries.get(path);
    DefinitionResourceDetail {
        path: path.to_owned(),
        kind: kind.to_string(),
        name,
        origin: effective_source.map(|_| {
            if persisted_source.is_some() {
                ResourceOrigin::Filesystem
            } else {
                ResourceOrigin::Bundled
            }
        }),
        active_origin: active.map(|entry| entry.origin),
        persisted: persisted_source.is_some(),
        effective: effective_source.is_some(),
        active: active.is_some(),
        source: effective_source.map(str::to_owned),
        persisted_source: persisted_source.cloned(),
        active_source: active.map(|entry| entry.source.clone()),
        persisted_etag: slot_etag(persisted_source.map(String::as_bytes)),
        effective_etag: effective_source.map(|source| source_etag(source.as_bytes())),
        active_etag: active.map(|entry| entry.etag.clone()),
        reload_error,
    }
}

fn bundled_resources() -> BTreeMap<String, String> {
    BUNDLED_RESOURCES
        .into_iter()
        .map(|(path, source)| (path.to_owned(), source.to_owned()))
        .collect()
}

fn unified_bundled_resources() -> BTreeMap<String, String> {
    UNIFIED_BUNDLED_RESOURCES
        .into_iter()
        .map(|(path, source)| (path.to_owned(), source.to_owned()))
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "snapshot construction keeps cross-kind materialization and atomic validation together"
)]
fn build_snapshot(
    bundled: BTreeMap<String, String>,
    persisted: &BTreeMap<String, String>,
    external_sources: &[String],
    default_agent: &str,
    materializer: Option<&CatalogMaterializer>,
) -> Result<AgentDefinitionSnapshot, AgentDefinitionLoadError> {
    let persisted_fingerprint = tree_fingerprint(persisted);
    let mut effective = bundled;
    for (path, source) in persisted {
        effective.insert(path.clone(), source.clone());
    }
    let mut identities = BTreeSet::new();
    let mut entries = BTreeMap::new();
    let mut sources = Vec::new();
    let mut configuration = config_proto::Configuration::default();
    let mut routines = BTreeMap::new();
    for (path, source) in effective {
        let key = validate_resource_source(&path, &source)?;
        if !identities.insert((key.kind, key.name.clone())) {
            return Err(AgentDefinitionLoadError::DuplicateIdentity {
                kind: key.kind,
                name: key.name,
            });
        }
        let origin = if persisted.contains_key(&path) {
            ResourceOrigin::Filesystem
        } else {
            ResourceOrigin::Bundled
        };
        let effective_value = match key.kind {
            ResourceKind::Agent | ResourceKind::FeatureSet | ResourceKind::PermissionSet => {
                sources.push(source.clone());
                yaml_json(&source, &path)?
            }
            ResourceKind::Provider => {
                let provider = materialize_provider(&source, &path, materializer)?;
                configuration
                    .providers
                    .insert(key.name.clone(), provider.clone());
                serde_json::to_value(provider).expect("generated provider serializes")
            }
            ResourceKind::McpServer => {
                let server = materialize_mcp_server(&source, &path)?;
                configuration
                    .mcp_servers
                    .insert(key.name.clone(), server.clone());
                serde_json::to_value(server).expect("generated MCP server serializes")
            }
            ResourceKind::Defaults => {
                let defaults = materialize_defaults(&source, &path)?;
                configuration.inference_defaults = Some(defaults.clone());
                serde_json::to_value(defaults).expect("generated defaults serialize")
            }
            ResourceKind::Routine => {
                if routines.len() >= MAX_ROUTINE_COUNT {
                    return Err(invalid_value(
                        &path,
                        format!("routine catalog exceeds {MAX_ROUTINE_COUNT} entries"),
                    ));
                }
                let definition = materialize_routine(&source, &path)?;
                let value = serde_json::to_value(&definition)
                    .expect("validated routine definition serializes");
                routines.insert(
                    key.name.clone(),
                    MaterializedRoutine::new(
                        key.name.clone(),
                        path.clone(),
                        source_etag(source.as_bytes()),
                        definition,
                    ),
                );
                value
            }
        };
        entries.insert(
            path,
            ActiveResource {
                etag: source_etag(source.as_bytes()),
                source: source.clone(),
                origin,
                kind: key.kind,
                name: key.name,
                effective_value,
            },
        );
    }
    sources.extend_from_slice(external_sources);

    let catalog = compile_catalog(&sources, default_agent)?;
    let etag = materialized_snapshot_etag(&entries, &configuration);
    if let Some(defaults) = &configuration.inference_defaults {
        let provider = configuration
            .providers
            .get(&defaults.provider_key)
            .ok_or_else(|| {
                invalid_value(
                    "defaults/default.yaml",
                    format!(
                        "default provider `{}` does not exist",
                        defaults.provider_key
                    ),
                )
            })?;
        if !provider.models.contains_key(&defaults.model_key) {
            return Err(invalid_value(
                "defaults/default.yaml",
                format!(
                    "default model `{}/{}` does not exist",
                    defaults.provider_key, defaults.model_key
                ),
            ));
        }
    }
    configuration.etag.clone_from(&etag);
    Ok(AgentDefinitionSnapshot {
        catalog,
        default_agent: default_agent.to_owned(),
        sources,
        entries,
        persisted_fingerprint,
        configuration,
        routines,
        etag,
    })
}

#[cfg(test)]
mod tests;
