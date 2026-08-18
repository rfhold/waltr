//! Strict YAML agent definitions and reference resolution.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    fmt::Write as _,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    PermissionCommandRule, PermissionDirectoryRule, PermissionEffect, PermissionInputRule,
    SystemPromptBuilder,
};

mod parse;

#[cfg(test)]
mod tests;

/// API version accepted by the agent-definition parser.
pub const AGENT_DEFINITION_API_VERSION: &str = "waltr.dev/v1alpha1";

/// Runtime expression accepted wherever a feature or permission requires the session workspace.
pub const SESSION_WORKSPACE_ROOT: &str = "$(session.workspace.root)";

/// Self-contained durable inputs needed to resolve one selected agent definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinitionBundle {
    /// Agent name selected from the bundled definitions.
    pub agent_name: String,
    /// YAML sources loaded in order, with later sources replacing matching resources.
    pub sources: Vec<String>,
    /// Pending coalesced selection audit, consumed at the next runtime capture boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_switch_audit: Option<AgentSwitchAudit>,
}

impl AgentDefinitionBundle {
    /// Create a durable agent-definition bundle.
    #[must_use]
    pub fn new(agent_name: impl Into<String>, sources: Vec<String>) -> Self {
        Self {
            agent_name: agent_name.into(),
            sources,
            pending_switch_audit: None,
        }
    }

    /// Return a deterministic, secret-free digest identifying this selected definition bundle.
    #[must_use]
    pub fn identity(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"waltr-agent-definition-v1\0");
        digest.update(self.agent_name.as_bytes());
        for source in &self.sources {
            digest.update((source.len() as u64).to_be_bytes());
            digest.update(source.as_bytes());
        }
        let mut identity = String::with_capacity(64);
        for byte in digest.finalize() {
            let _ = write!(identity, "{byte:02x}");
        }
        identity
    }

    /// Reload the bundled sources in order and resolve the selected agent.
    ///
    /// # Errors
    ///
    /// Returns an error when a source is malformed or unsupported, or when the selected agent and
    /// its references cannot be resolved.
    pub fn resolve(&self) -> Result<ResolvedAgent, AgentDefinitionError> {
        let mut registry = AgentDefinitionRegistry::new();
        for source in &self.sources {
            registry.load_yaml(source)?;
        }
        registry.resolve_agent(&self.agent_name)
    }
}

/// Durable, source-free marker for one pending agent selection audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSwitchAudit {
    /// Stable marker identity used to make boundary consumption retry-safe.
    pub id: String,
    /// Effective agent before the first coalesced switch.
    pub old_name: String,
    /// Effective agent after the latest coalesced switch.
    pub new_name: String,
}

/// Public metadata for one selectable startup agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCatalogEntry {
    /// Selectable agent name.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
}

/// Deterministic catalog of selectable agents backed by one ordered source set.
#[derive(Debug, Clone)]
pub struct AgentDefinitionCatalog {
    default_agent: String,
    sources: Vec<String>,
    agents: BTreeMap<String, ResolvedAgent>,
}

impl AgentDefinitionCatalog {
    /// Build a catalog from already loaded sources and a validated default name.
    ///
    /// # Errors
    ///
    /// Returns an error when a source is invalid, any agent cannot be resolved, or the requested
    /// default agent is absent.
    pub fn from_sources(
        sources: Vec<String>,
        default_agent: impl Into<String>,
    ) -> Result<Self, AgentDefinitionError> {
        let default_agent = default_agent.into();
        let mut registry = AgentDefinitionRegistry::new();
        for source in &sources {
            registry.load_yaml(source)?;
        }
        let agents = registry
            .resolve_agents()?
            .into_iter()
            .map(|agent| (agent.name.clone(), agent))
            .collect::<BTreeMap<_, _>>();
        if !agents.contains_key(&default_agent) {
            return Err(AgentDefinitionError::MissingReference {
                kind: AgentDefinitionResourceKind::Agent,
                name: default_agent,
                field: None,
            });
        }
        Ok(Self {
            default_agent,
            sources,
            agents,
        })
    }

    /// Build a catalog from host-validated resolved agents and their exact ordered sources.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested default agent is absent from `agents`.
    pub fn from_resolved_agents(
        sources: Vec<String>,
        default_agent: impl Into<String>,
        agents: impl IntoIterator<Item = ResolvedAgent>,
    ) -> Result<Self, AgentDefinitionError> {
        let default_agent = default_agent.into();
        let agents = agents
            .into_iter()
            .map(|agent| (agent.name.clone(), agent))
            .collect::<BTreeMap<_, _>>();
        if !agents.contains_key(&default_agent) {
            return Err(AgentDefinitionError::MissingReference {
                kind: AgentDefinitionResourceKind::Agent,
                name: default_agent,
                field: None,
            });
        }
        Ok(Self {
            default_agent,
            sources,
            agents,
        })
    }

    /// Return the configured default agent name.
    #[must_use]
    pub fn default_agent_name(&self) -> &str {
        &self.default_agent
    }

    /// List selectable agents in deterministic name order.
    #[must_use]
    pub fn entries(&self) -> Vec<AgentCatalogEntry> {
        self.agents
            .values()
            .map(|agent| AgentCatalogEntry {
                name: agent.name.clone(),
                description: agent.description.clone(),
            })
            .collect()
    }

    /// Resolve a currently selectable agent by name.
    #[must_use]
    pub fn resolved(&self, name: &str) -> Option<&ResolvedAgent> {
        self.agents.get(name)
    }

    /// Build a fresh self-contained bundle for a currently selectable name.
    #[must_use]
    pub fn bundle(&self, name: &str) -> Option<AgentDefinitionBundle> {
        self.agents
            .contains_key(name)
            .then(|| AgentDefinitionBundle::new(name, self.sources.clone()))
    }

    /// Build the default agent's self-contained bundle.
    ///
    /// # Panics
    ///
    /// Panics if the catalog no longer contains its declared default agent.
    #[must_use]
    pub fn default_bundle(&self) -> AgentDefinitionBundle {
        self.bundle(&self.default_agent)
            .expect("catalog default agent must remain present")
    }
}

/// Kinds of resources accepted by the agent-definition registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentDefinitionResourceKind {
    /// An agent definition.
    Agent,
    /// A reusable feature set.
    FeatureSet,
    /// A reusable permission set.
    PermissionSet,
}

impl fmt::Display for AgentDefinitionResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Agent => "Agent",
            Self::FeatureSet => "FeatureSet",
            Self::PermissionSet => "PermissionSet",
        })
    }
}

/// Identity of one resource in an agent-definition YAML source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinitionResourceKey {
    /// Resource kind.
    pub kind: AgentDefinitionResourceKind,
    /// Resource `metadata.name`.
    pub name: String,
}

/// Strictly parse an agent-definition YAML source and return its resource keys.
///
/// Keys are returned in YAML document order. Parsing validates each complete resource and rejects
/// duplicate `(kind, metadata.name)` keys within the source.
///
/// # Errors
///
/// Returns the same errors as [`AgentDefinitionRegistry::load_yaml`] for malformed, unsupported,
/// invalid, or duplicate resources.
pub fn inspect_agent_definition_source(
    source: &str,
) -> Result<Vec<AgentDefinitionResourceKey>, AgentDefinitionError> {
    parse::parse_source(source).map(|resources| {
        resources
            .into_iter()
            .map(|resource| AgentDefinitionResourceKey {
                kind: resource.kind(),
                name: resource.name().to_owned(),
            })
            .collect()
    })
}

/// Failure while parsing, registering, or resolving agent definitions.
#[derive(Debug)]
pub enum AgentDefinitionError {
    /// One YAML document could not be deserialized.
    Yaml {
        /// One-based document number within the source.
        document: usize,
        /// YAML deserialization failure.
        source: serde_yaml_ng::Error,
    },
    /// A resource declared an API version this library does not implement.
    UnsupportedApiVersion {
        /// One-based document number within the source.
        document: usize,
        /// Declared API version.
        api_version: String,
    },
    /// A supported field contained a value outside the implemented definition subset.
    InvalidValue {
        /// One-based document number within the source.
        document: usize,
        /// Field whose value was invalid.
        field: String,
        /// Validation failure description.
        message: String,
    },
    /// One parsed source declared the same resource key more than once.
    DuplicateResource {
        /// One-based document number containing the duplicate.
        document: usize,
        /// Duplicate resource kind.
        kind: AgentDefinitionResourceKind,
        /// Duplicate resource name.
        name: String,
    },
    /// Resolution could not find a referenced resource.
    MissingReference {
        /// Missing resource kind.
        kind: AgentDefinitionResourceKind,
        /// Missing resource name.
        name: String,
        /// Definition field containing the reference, when applicable.
        field: Option<String>,
    },
    /// Agent inheritance contains a cycle.
    InheritanceCycle {
        /// Ordered cycle, with the first agent repeated at the end.
        agents: Vec<String>,
    },
}

impl fmt::Display for AgentDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yaml { document, source } => {
                write!(
                    formatter,
                    "invalid agent-definition YAML document {document}: {source}"
                )
            }
            Self::UnsupportedApiVersion {
                document,
                api_version,
            } => write!(
                formatter,
                "unsupported apiVersion `{api_version}` in agent-definition YAML document {document}"
            ),
            Self::InvalidValue {
                document,
                field,
                message,
            } => write!(
                formatter,
                "invalid `{field}` in agent-definition YAML document {document}: {message}"
            ),
            Self::DuplicateResource {
                document,
                kind,
                name,
            } => write!(
                formatter,
                "duplicate {kind} resource `{name}` in agent-definition YAML document {document}"
            ),
            Self::MissingReference { kind, name, field } => match field {
                Some(field) => write!(formatter, "missing {kind} reference `{name}` at `{field}`"),
                None => write!(formatter, "missing {kind} reference `{name}`"),
            },
            Self::InheritanceCycle { agents } => write!(
                formatter,
                "agent inheritance cycle: {}",
                agents.join(" -> ")
            ),
        }
    }
}

impl Error for AgentDefinitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Yaml { source, .. } => Some(source),
            Self::UnsupportedApiVersion { .. }
            | Self::InvalidValue { .. }
            | Self::DuplicateResource { .. }
            | Self::MissingReference { .. }
            | Self::InheritanceCycle { .. } => None,
        }
    }
}

/// Feature names supported by agent definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentFeature {
    /// Filesystem tools and their prompt context.
    Filesystem,
    /// Hierarchical `AGENTS.md` instructions.
    Agents,
    /// Agent Skills discovery and context.
    Skills,
    /// Shell execution and prompt context.
    Shell,
    /// Subagent task execution.
    Task,
    /// Tools and context supplied by configured MCP servers.
    Mcp,
}

impl AgentFeature {
    /// Return the YAML name for this feature.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
            Self::Agents => "agents",
            Self::Skills => "skills",
            Self::Shell => "shell",
            Self::Task => "task",
            Self::Mcp => "mcp",
        }
    }
}

/// Filesystem tool profile supported by definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemFeatureProfile {
    /// Select filesystem tools from the runtime model.
    Auto,
}

/// Resolved filesystem feature configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct FilesystemFeatureConfig {
    /// Runtime workspace expression used as the filesystem root.
    pub root: String,
    /// Filesystem tool profile.
    pub profile: FilesystemFeatureProfile,
    /// Whether to add filesystem-specific system prompt context.
    pub system_prompt: bool,
    /// Whether reads reveal applicable `AGENTS.md` context.
    pub agents_read_context: bool,
    /// Whether filesystem operations reveal Agent Skills context.
    pub skills_context: bool,
    /// Whether to install the feature's runtime-owned default permissions.
    pub default_permissions: bool,
}

/// Resolved `AGENTS.md` feature configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsFeatureConfig {
    /// Runtime workspace expression used as the search root.
    pub root: String,
    /// Instruction filename.
    pub filename: String,
    /// Whether to add discovered instructions to the system prompt.
    pub system_prompt: bool,
}

/// Resolved Agent Skills feature configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsFeatureConfig {
    /// Runtime workspace expression used as the discovery root.
    pub root: String,
    /// Whether to install the feature's runtime-owned default permissions.
    pub default_permissions: bool,
}

/// Resolved shell feature configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellFeatureConfig {
    /// Runtime workspace expression used as the shell root.
    pub root: String,
    /// Whether to add shell-specific system prompt context.
    pub system_prompt: bool,
    /// Whether to install the feature's runtime-owned default permissions.
    pub default_permissions: bool,
}

/// Resolved task feature configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFeatureConfig {
    /// Maximum number of subagent levels that may be created.
    pub max_subagent_depth: usize,
    /// Whether to install the feature's runtime-owned default permissions.
    pub default_permissions: bool,
}

/// Resolved MCP feature configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerSelection {
    /// Select only the named configured MCP servers.
    Include(BTreeSet<String>),
    /// Select every configured MCP server except the named servers.
    Exclude(BTreeSet<String>),
}

impl Default for McpServerSelection {
    fn default() -> Self {
        Self::Exclude(BTreeSet::new())
    }
}

/// Resolved MCP feature configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct McpFeatureConfig {
    /// Configured MCP servers selected by this definition. Omission selects all servers.
    pub servers: McpServerSelection,
    /// Whether configured MCP tools are exposed to the model.
    pub expose_tools: bool,
    /// Whether MCP server instructions contribute prompt context.
    pub instructions: bool,
    /// Whether MCP resources contribute request context.
    pub resources: bool,
    /// Whether MCP prompts are available to the agent.
    pub prompts: bool,
    /// Whether trace context is propagated to MCP servers.
    pub trace_propagation: bool,
    /// Whether to install the feature's runtime-owned default permissions.
    pub default_permissions: bool,
}

/// Feature configuration after inheritance and feature-set composition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedAgentFeatures {
    /// Final filesystem feature configuration, when enabled.
    pub filesystem: Option<FilesystemFeatureConfig>,
    /// Final `AGENTS.md` feature configuration, when enabled.
    pub agents: Option<AgentsFeatureConfig>,
    /// Final Agent Skills feature configuration, when enabled.
    pub skills: Option<SkillsFeatureConfig>,
    /// Final shell feature configuration, when enabled.
    pub shell: Option<ShellFeatureConfig>,
    /// Final task feature configuration, when enabled.
    pub task: Option<TaskFeatureConfig>,
    /// Final MCP feature configuration, when enabled.
    pub mcp: Option<McpFeatureConfig>,
}

impl ResolvedAgentFeatures {
    fn apply(&mut self, later: &Self) {
        if let Some(config) = &later.filesystem {
            self.filesystem = Some(config.clone());
        }
        if let Some(config) = &later.agents {
            self.agents = Some(config.clone());
        }
        if let Some(config) = &later.skills {
            self.skills = Some(config.clone());
        }
        if let Some(config) = &later.shell {
            self.shell = Some(config.clone());
        }
        if let Some(config) = &later.task {
            self.task = Some(config.clone());
        }
        if let Some(config) = &later.mcp {
            self.mcp = Some(config.clone());
        }
    }
}

/// Selector attached to one resolved permission declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentPermissionSelector {
    /// Select exactly one provider-facing tool name.
    Name(String),
    /// Select every tool generated by one feature.
    Feature(AgentFeature),
}

/// Resource rules from one structured permission declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPermissionResources {
    /// Ordered path rules.
    pub paths: Vec<PermissionDirectoryRule>,
    /// Ordered executable-name rules.
    pub commands: Vec<PermissionCommandRule>,
}

/// Permission attached to a resolved tool or feature selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPermission {
    /// Whole-tool allow, ask, or deny effect.
    Tool(PermissionEffect),
    /// Resource-specific path and command effects.
    Resources(ResolvedPermissionResources),
    /// Strict nested tool-input effects.
    Inputs(Vec<PermissionInputRule>),
}

/// One resolved permission declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPermissionEntry {
    /// Tool name or feature selector.
    pub selector: AgentPermissionSelector,
    /// Permission declared for the selector.
    pub permission: ResolvedPermission,
}

/// Agent definition after inheritance and all references have resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    /// Resolved agent name.
    pub name: String,
    /// Child-most description, inheriting the parent description when absent.
    pub description: Option<String>,
    /// Composed system prompt.
    ///
    /// Each non-empty parent-to-child fragment is trimmed, then fragments are joined with `\n\n`.
    pub system_prompt: Option<String>,
    /// Feature configuration after ordered, feature-granularity replacement.
    pub features: ResolvedAgentFeatures,
    /// Permission declarations in inherited-reference then child-reference order.
    pub permissions: Vec<ResolvedPermissionEntry>,
}

impl ResolvedAgent {
    fn empty(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            description: None,
            system_prompt: None,
            features: ResolvedAgentFeatures::default(),
            permissions: Vec::new(),
        }
    }
}

/// In-memory registry for YAML agent-definition resources.
#[derive(Debug, Clone, Default)]
pub struct AgentDefinitionRegistry {
    agents: BTreeMap<String, AgentDefinition>,
    feature_sets: BTreeMap<String, FeatureSetDefinition>,
    permission_sets: BTreeMap<String, PermissionSetDefinition>,
}

impl AgentDefinitionRegistry {
    /// Create an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            agents: BTreeMap::new(),
            feature_sets: BTreeMap::new(),
            permission_sets: BTreeMap::new(),
        }
    }

    /// Strictly parse and load every YAML document in one source.
    ///
    /// The operation is atomic. Duplicate resource keys in this source fail before any resource is
    /// inserted. A successful call replaces resources with matching `(kind, metadata.name)` keys
    /// from earlier calls, so callers control source precedence by call order.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed YAML, unknown fields or values, unsupported constructs, or
    /// duplicate resources within this source.
    pub fn load_yaml(&mut self, source: &str) -> Result<(), AgentDefinitionError> {
        let resources = parse::parse_source(source)?;
        for resource in resources {
            match resource {
                DefinitionResource::Agent { name, definition } => {
                    self.agents.insert(name, definition);
                }
                DefinitionResource::FeatureSet { name, definition } => {
                    self.feature_sets.insert(name, definition);
                }
                DefinitionResource::PermissionSet { name, definition } => {
                    self.permission_sets.insert(name, definition);
                }
            }
        }
        Ok(())
    }

    /// Resolve one named agent against the registry's current resources.
    ///
    /// # Errors
    ///
    /// Returns an error when the agent, an inherited agent, or a referenced set is missing, or when
    /// agent inheritance contains a cycle.
    pub fn resolve_agent(&self, name: &str) -> Result<ResolvedAgent, AgentDefinitionError> {
        self.resolve_agent_inner(name, &mut Vec::new(), &mut BTreeMap::new())
    }

    /// Resolve every agent in deterministic name order.
    ///
    /// # Errors
    ///
    /// Returns an error when any final agent definition has a missing reference or inheritance
    /// cycle.
    pub fn resolve_agents(&self) -> Result<Vec<ResolvedAgent>, AgentDefinitionError> {
        let mut cache = BTreeMap::new();
        self.agents
            .keys()
            .map(|name| self.resolve_agent_inner(name, &mut Vec::new(), &mut cache))
            .collect()
    }

    fn resolve_agent_inner(
        &self,
        name: &str,
        stack: &mut Vec<String>,
        cache: &mut BTreeMap<String, ResolvedAgent>,
    ) -> Result<ResolvedAgent, AgentDefinitionError> {
        if let Some(resolved) = cache.get(name) {
            return Ok(resolved.clone());
        }
        if let Some(start) = stack.iter().position(|candidate| candidate == name) {
            let mut agents = stack[start..].to_vec();
            agents.push(name.to_owned());
            return Err(AgentDefinitionError::InheritanceCycle { agents });
        }
        let definition = self.agents.get(name).cloned().ok_or_else(|| {
            AgentDefinitionError::MissingReference {
                kind: AgentDefinitionResourceKind::Agent,
                name: name.to_owned(),
                field: None,
            }
        })?;

        stack.push(name.to_owned());
        let result = self.compose_agent(name, &definition, stack, cache);
        stack.pop();
        let resolved = result?;
        cache.insert(name.to_owned(), resolved.clone());
        Ok(resolved)
    }

    fn compose_agent(
        &self,
        name: &str,
        definition: &AgentDefinition,
        stack: &mut Vec<String>,
        cache: &mut BTreeMap<String, ResolvedAgent>,
    ) -> Result<ResolvedAgent, AgentDefinitionError> {
        let mut resolved = if let Some(parent) = &definition.inherits_from {
            self.resolve_agent_inner(parent, stack, cache)?
        } else {
            ResolvedAgent::empty(name)
        };
        name.clone_into(&mut resolved.name);
        if let Some(description) = &definition.description {
            resolved.description = Some(description.clone());
        }
        resolved.system_prompt = compose_prompt(
            resolved.system_prompt.as_deref(),
            definition.system_prompt.as_deref(),
        );

        for (index, entry) in definition.features.iter().enumerate() {
            match entry {
                AgentFeatureEntry::FeatureSetRef(reference) => {
                    let feature_set = self.feature_sets.get(reference).ok_or_else(|| {
                        AgentDefinitionError::MissingReference {
                            kind: AgentDefinitionResourceKind::FeatureSet,
                            name: reference.clone(),
                            field: Some(format!("spec.features[{index}].featureSetRef.name")),
                        }
                    })?;
                    resolved.features.apply(&feature_set.features);
                }
                AgentFeatureEntry::Inline(features) => resolved.features.apply(features),
            }
        }
        for (index, entry) in definition.permissions.iter().enumerate() {
            match entry {
                AgentPermissionEntry::PermissionSetRef(reference) => {
                    let permission_set = self.permission_sets.get(reference).ok_or_else(|| {
                        AgentDefinitionError::MissingReference {
                            kind: AgentDefinitionResourceKind::PermissionSet,
                            name: reference.clone(),
                            field: Some(format!("spec.permissions[{index}].permissionSetRef.name")),
                        }
                    })?;
                    resolved
                        .permissions
                        .extend(permission_set.permissions.iter().cloned());
                }
                AgentPermissionEntry::Inline(permission) => {
                    resolved.permissions.push(permission.clone());
                }
            }
        }
        Ok(resolved)
    }
}

fn compose_prompt(parent: Option<&str>, child: Option<&str>) -> Option<String> {
    let mut prompt = SystemPromptBuilder::new();
    if let Some(parent) = parent {
        prompt = prompt.with_fragment(parent);
    }
    if let Some(child) = child {
        prompt = prompt.with_fragment(child);
    }
    prompt.build()
}

#[derive(Debug, Clone)]
struct AgentDefinition {
    description: Option<String>,
    system_prompt: Option<String>,
    inherits_from: Option<String>,
    features: Vec<AgentFeatureEntry>,
    permissions: Vec<AgentPermissionEntry>,
}

#[derive(Debug, Clone)]
enum AgentFeatureEntry {
    FeatureSetRef(String),
    Inline(Box<ResolvedAgentFeatures>),
}

#[derive(Debug, Clone)]
enum AgentPermissionEntry {
    PermissionSetRef(String),
    Inline(ResolvedPermissionEntry),
}

#[derive(Debug, Clone)]
struct FeatureSetDefinition {
    features: ResolvedAgentFeatures,
}

#[derive(Debug, Clone)]
struct PermissionSetDefinition {
    permissions: Vec<ResolvedPermissionEntry>,
}

#[derive(Debug)]
enum DefinitionResource {
    Agent {
        name: String,
        definition: AgentDefinition,
    },
    FeatureSet {
        name: String,
        definition: FeatureSetDefinition,
    },
    PermissionSet {
        name: String,
        definition: PermissionSetDefinition,
    },
}

impl DefinitionResource {
    const fn kind(&self) -> AgentDefinitionResourceKind {
        match self {
            Self::Agent { .. } => AgentDefinitionResourceKind::Agent,
            Self::FeatureSet { .. } => AgentDefinitionResourceKind::FeatureSet,
            Self::PermissionSet { .. } => AgentDefinitionResourceKind::PermissionSet,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Agent { name, .. }
            | Self::FeatureSet { name, .. }
            | Self::PermissionSet { name, .. } => name,
        }
    }
}
