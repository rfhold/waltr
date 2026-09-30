use std::{collections::BTreeSet, path::PathBuf};

use serde::{
    Deserialize, Deserializer,
    de::{Error as _, SeqAccess, Visitor},
};

use crate::{
    PermissionCommandPattern, PermissionCommandRule, PermissionDirectoryRule, PermissionEffect,
    PermissionInputCondition, PermissionInputRule, PermissionInputScalar,
    PermissionInputValueMatcher, PermissionJsonPointer, PermissionPathPattern,
};

use super::{
    AGENT_DEFINITION_API_VERSION, AgentDefinition, AgentDefinitionError, AgentFeature,
    AgentFeatureEntry, AgentPermissionEntry, AgentPermissionSelector, AgentsFeatureConfig,
    DefinitionResource, FeatureSetDefinition, FilesystemFeatureConfig, FilesystemFeatureProfile,
    McpFeatureConfig, McpServerSelection, PermissionSetDefinition, ResolvedAgentFeatures,
    ResolvedPermission, ResolvedPermissionEntry, ResolvedPermissionResources,
    SESSION_WORKSPACE_ROOT, ShellFeatureConfig, SkillsFeatureConfig, TaskFeatureConfig,
};

pub(super) fn parse_source(source: &str) -> Result<Vec<DefinitionResource>, AgentDefinitionError> {
    let mut resources = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, document) in serde_yaml_ng::Deserializer::from_str(source).enumerate() {
        let document_number = index + 1;
        let raw =
            RawResource::deserialize(document).map_err(|source| AgentDefinitionError::Yaml {
                document: document_number,
                source,
            })?;
        let resource = raw.resolve(document_number)?;
        let key = (resource.kind(), resource.name().to_owned());
        if !seen.insert(key.clone()) {
            return Err(AgentDefinitionError::DuplicateResource {
                document: document_number,
                kind: key.0,
                name: key.1,
            });
        }
        resources.push(resource);
    }
    Ok(resources)
}

#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum RawResource {
    Agent {
        #[serde(rename = "apiVersion")]
        api_version: String,
        metadata: RawMetadata,
        spec: RawAgentSpec,
    },
    FeatureSet {
        #[serde(rename = "apiVersion")]
        api_version: String,
        metadata: RawMetadata,
        spec: RawFeatureSetSpec,
    },
    PermissionSet {
        #[serde(rename = "apiVersion")]
        api_version: String,
        metadata: RawMetadata,
        spec: RawPermissionSetSpec,
    },
}

impl RawResource {
    fn resolve(self, document: usize) -> Result<DefinitionResource, AgentDefinitionError> {
        match self {
            Self::Agent {
                api_version,
                metadata,
                spec,
            } => {
                validate_api_version(document, api_version)?;
                let name = metadata.resolve(document)?;
                Ok(DefinitionResource::Agent {
                    name,
                    definition: spec.resolve(document)?,
                })
            }
            Self::FeatureSet {
                api_version,
                metadata,
                spec,
            } => {
                validate_api_version(document, api_version)?;
                let name = metadata.resolve(document)?;
                Ok(DefinitionResource::FeatureSet {
                    name,
                    definition: FeatureSetDefinition {
                        features: spec.resolve(document)?,
                    },
                })
            }
            Self::PermissionSet {
                api_version,
                metadata,
                spec,
            } => {
                validate_api_version(document, api_version)?;
                let name = metadata.resolve(document)?;
                Ok(DefinitionResource::PermissionSet {
                    name,
                    definition: PermissionSetDefinition {
                        permissions: spec.resolve(document)?,
                    },
                })
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetadata {
    name: String,
}

impl RawMetadata {
    fn resolve(self, document: usize) -> Result<String, AgentDefinitionError> {
        non_empty(document, "metadata.name", self.name)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawAgentSpec {
    #[serde(default, deserialize_with = "deserialize_present")]
    description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    system_prompt: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    inherits_from: Option<RawReference>,
    #[serde(default)]
    features: RawAgentFeatureEntries,
    #[serde(default)]
    permissions: RawAgentPermissionEntries,
}

impl RawAgentSpec {
    fn resolve(self, document: usize) -> Result<AgentDefinition, AgentDefinitionError> {
        Ok(AgentDefinition {
            description: self.description,
            system_prompt: self.system_prompt,
            inherits_from: self
                .inherits_from
                .map(|reference| reference.resolve(document, "spec.inheritsFrom.name"))
                .transpose()?,
            features: self
                .features
                .0
                .into_iter()
                .enumerate()
                .map(|(index, entry)| entry.resolve(document, index))
                .collect::<Result<_, _>>()?,
            permissions: self
                .permissions
                .0
                .into_iter()
                .enumerate()
                .map(|(index, entry)| entry.resolve(document, index))
                .collect::<Result<_, _>>()?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReference {
    name: String,
}

impl RawReference {
    fn resolve(
        self,
        document: usize,
        field: impl Into<String>,
    ) -> Result<String, AgentDefinitionError> {
        non_empty(document, field, self.name)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentFeatureEntry {
    #[serde(
        rename = "featureSetRef",
        default,
        deserialize_with = "deserialize_present"
    )]
    feature_set_ref: Option<RawReference>,
    #[serde(default, deserialize_with = "deserialize_present")]
    filesystem: Option<RawFilesystemFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    agents: Option<RawAgentsFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    skills: Option<RawSkillsFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    shell: Option<RawShellFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    task: Option<RawTaskFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    mcp: Option<RawMcpFeature>,
}

#[derive(Default)]
struct RawAgentFeatureEntries(Vec<RawAgentFeatureEntry>);

impl<'de> Deserialize<'de> for RawAgentFeatureEntries {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = RawAgentFeatureEntries;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an ordered feature list")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                loop {
                    let index = entries.len();
                    let entry =
                        sequence
                            .next_element::<RawAgentFeatureEntry>()
                            .map_err(|error| {
                                A::Error::custom(format!("spec.features[{index}]: {error}"))
                            })?;
                    let Some(entry) = entry else {
                        break;
                    };
                    entries.push(entry);
                }
                Ok(RawAgentFeatureEntries(entries))
            }
        }

        deserializer.deserialize_seq(EntriesVisitor)
    }
}

impl RawAgentFeatureEntry {
    fn resolve(
        self,
        document: usize,
        index: usize,
    ) -> Result<AgentFeatureEntry, AgentDefinitionError> {
        let field = format!("spec.features[{index}]");
        let count = usize::from(self.feature_set_ref.is_some())
            + usize::from(self.filesystem.is_some())
            + usize::from(self.agents.is_some())
            + usize::from(self.skills.is_some())
            + usize::from(self.shell.is_some())
            + usize::from(self.task.is_some())
            + usize::from(self.mcp.is_some());
        if count != 1 {
            return Err(invalid_value(
                document,
                field,
                "exactly one feature variant is required",
            ));
        }
        if let Some(reference) = self.feature_set_ref {
            return reference
                .resolve(document, format!("{field}.featureSetRef.name"))
                .map(AgentFeatureEntry::FeatureSetRef);
        }
        let mut features = ResolvedAgentFeatures::default();
        if let Some(config) = self.filesystem {
            features.filesystem = Some(config.resolve(document, &format!("{field}.filesystem"))?);
        } else if let Some(config) = self.agents {
            features.agents = Some(config.resolve(document, &format!("{field}.agents"))?);
        } else if let Some(config) = self.skills {
            features.skills = Some(config.resolve(document, &format!("{field}.skills"))?);
        } else if let Some(config) = self.shell {
            features.shell = Some(config.resolve(document, &format!("{field}.shell"))?);
        } else if let Some(config) = self.task {
            features.task = Some(config.resolve());
        } else if let Some(config) = self.mcp {
            features.mcp = Some(config.resolve(document, &format!("{field}.mcp"))?);
        }
        Ok(AgentFeatureEntry::Inline(Box::new(features)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentPermissionEntry {
    #[serde(
        rename = "permissionSetRef",
        default,
        deserialize_with = "deserialize_present"
    )]
    permission_set_ref: Option<RawReference>,
    #[serde(default, deserialize_with = "deserialize_present")]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    feature: Option<RawAgentFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    permission: Option<RawPermission>,
}

#[derive(Default)]
struct RawAgentPermissionEntries(Vec<RawAgentPermissionEntry>);

impl<'de> Deserialize<'de> for RawAgentPermissionEntries {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = RawAgentPermissionEntries;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an ordered permission list")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                loop {
                    let index = entries.len();
                    let entry =
                        sequence
                            .next_element::<RawAgentPermissionEntry>()
                            .map_err(|error| {
                                A::Error::custom(format!("spec.permissions[{index}]: {error}"))
                            })?;
                    let Some(entry) = entry else {
                        break;
                    };
                    entries.push(entry);
                }
                Ok(RawAgentPermissionEntries(entries))
            }
        }

        deserializer.deserialize_seq(EntriesVisitor)
    }
}

impl RawAgentPermissionEntry {
    fn resolve(
        self,
        document: usize,
        index: usize,
    ) -> Result<AgentPermissionEntry, AgentDefinitionError> {
        let field = format!("spec.permissions[{index}]");
        if let Some(reference) = self.permission_set_ref {
            if self.name.is_some() || self.feature.is_some() || self.permission.is_some() {
                return Err(invalid_value(
                    document,
                    field,
                    "exactly one of `permissionSetRef` or an inline permission is required",
                ));
            }
            return reference
                .resolve(document, format!("{field}.permissionSetRef.name"))
                .map(AgentPermissionEntry::PermissionSetRef);
        }
        let permission = self.permission.ok_or_else(|| {
            invalid_value(document, &field, "inline permission requires `permission`")
        })?;
        resolve_permission_entry(document, &field, self.name, self.feature, permission)
            .map(AgentPermissionEntry::Inline)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFeatureSetSpec {
    #[serde(default, deserialize_with = "deserialize_present")]
    filesystem: Option<RawFilesystemFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    agents: Option<RawAgentsFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    skills: Option<RawSkillsFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    shell: Option<RawShellFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    task: Option<RawTaskFeature>,
    #[serde(default, deserialize_with = "deserialize_present")]
    mcp: Option<RawMcpFeature>,
}

impl RawFeatureSetSpec {
    fn resolve(self, document: usize) -> Result<ResolvedAgentFeatures, AgentDefinitionError> {
        Ok(ResolvedAgentFeatures {
            filesystem: self
                .filesystem
                .map(|config| config.resolve(document, "spec.filesystem"))
                .transpose()?,
            agents: self
                .agents
                .map(|config| config.resolve(document, "spec.agents"))
                .transpose()?,
            skills: self
                .skills
                .map(|config| config.resolve(document, "spec.skills"))
                .transpose()?,
            shell: self
                .shell
                .map(|config| config.resolve(document, "spec.shell"))
                .transpose()?,
            task: self.task.map(RawTaskFeature::resolve),
            mcp: self
                .mcp
                .map(|config| config.resolve(document, "spec.mcp"))
                .transpose()?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct RawFilesystemFeature {
    root: String,
    profile: RawFilesystemFeatureProfile,
    #[serde(default)]
    system_prompt: bool,
    #[serde(default)]
    agents_read_context: bool,
    #[serde(default)]
    skills_context: bool,
    #[serde(default)]
    default_permissions: bool,
}

impl RawFilesystemFeature {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<FilesystemFeatureConfig, AgentDefinitionError> {
        Ok(FilesystemFeatureConfig {
            root: workspace_root(document, format!("{field}.root"), self.root)?,
            profile: match self.profile {
                RawFilesystemFeatureProfile::Auto => FilesystemFeatureProfile::Auto,
            },
            system_prompt: self.system_prompt,
            agents_read_context: self.agents_read_context,
            skills_context: self.skills_context,
            default_permissions: self.default_permissions,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum RawFilesystemFeatureProfile {
    Auto,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawAgentsFeature {
    root: String,
    filename: String,
    #[serde(default)]
    system_prompt: bool,
}

impl RawAgentsFeature {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<AgentsFeatureConfig, AgentDefinitionError> {
        let filename = if self.filename == "AGENTS.md" {
            self.filename
        } else {
            return Err(invalid_value(
                document,
                format!("{field}.filename"),
                "only `AGENTS.md` is supported",
            ));
        };
        Ok(AgentsFeatureConfig {
            root: workspace_root(document, format!("{field}.root"), self.root)?,
            filename,
            system_prompt: self.system_prompt,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawSkillsFeature {
    root: String,
    #[serde(default)]
    default_permissions: bool,
}

impl RawSkillsFeature {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<SkillsFeatureConfig, AgentDefinitionError> {
        Ok(SkillsFeatureConfig {
            root: workspace_root(document, format!("{field}.root"), self.root)?,
            default_permissions: self.default_permissions,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawShellFeature {
    root: String,
    #[serde(default)]
    system_prompt: bool,
    #[serde(default)]
    default_permissions: bool,
}

impl RawShellFeature {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<ShellFeatureConfig, AgentDefinitionError> {
        Ok(ShellFeatureConfig {
            root: workspace_root(document, format!("{field}.root"), self.root)?,
            system_prompt: self.system_prompt,
            default_permissions: self.default_permissions,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawTaskFeature {
    max_subagent_depth: usize,
    #[serde(default)]
    default_permissions: bool,
}

impl RawTaskFeature {
    const fn resolve(self) -> TaskFeatureConfig {
        TaskFeatureConfig {
            max_subagent_depth: self.max_subagent_depth,
            default_permissions: self.default_permissions,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct RawMcpFeature {
    #[serde(default, deserialize_with = "deserialize_present")]
    servers: Option<RawMcpServerSelection>,
    #[serde(default)]
    expose_tools: bool,
    #[serde(default)]
    instructions: bool,
    #[serde(default)]
    resources: bool,
    #[serde(default)]
    prompts: bool,
    #[serde(default)]
    skills: bool,
    #[serde(default = "default_true")]
    trace_propagation: bool,
    #[serde(default)]
    default_permissions: bool,
}

impl RawMcpFeature {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<McpFeatureConfig, AgentDefinitionError> {
        Ok(McpFeatureConfig {
            servers: self
                .servers
                .map(|selection| selection.resolve(document, &format!("{field}.servers")))
                .transpose()?
                .unwrap_or_default(),
            expose_tools: self.expose_tools,
            instructions: self.instructions,
            resources: self.resources,
            prompts: self.prompts,
            skills: self.skills,
            trace_propagation: self.trace_propagation,
            default_permissions: self.default_permissions,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServerSelection {
    #[serde(default, deserialize_with = "deserialize_present")]
    include: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    exclude: Option<Vec<String>>,
}

impl RawMcpServerSelection {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<McpServerSelection, AgentDefinitionError> {
        match (self.include, self.exclude) {
            (Some(names), None) => resolve_server_names(document, field, "include", names)
                .map(McpServerSelection::Include),
            (None, Some(names)) => resolve_server_names(document, field, "exclude", names)
                .map(McpServerSelection::Exclude),
            (Some(_), Some(_)) | (None, None) => Err(invalid_value(
                document,
                field,
                "exactly one of `include` or `exclude` is required",
            )),
        }
    }
}

fn resolve_server_names(
    document: usize,
    field: &str,
    selector: &str,
    names: Vec<String>,
) -> Result<BTreeSet<String>, AgentDefinitionError> {
    let mut resolved = BTreeSet::new();
    for (index, name) in names.into_iter().enumerate() {
        let name = non_empty(document, format!("{field}.{selector}[{index}]"), name)?;
        if !resolved.insert(name.clone()) {
            return Err(invalid_value(
                document,
                format!("{field}.{selector}[{index}]"),
                format!("duplicate server name `{name}`"),
            ));
        }
    }
    Ok(resolved)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissionSetSpec {
    tools: Vec<RawPermissionEntry>,
}

impl RawPermissionSetSpec {
    fn resolve(
        self,
        document: usize,
    ) -> Result<Vec<ResolvedPermissionEntry>, AgentDefinitionError> {
        self.tools
            .into_iter()
            .enumerate()
            .map(|(index, entry)| entry.resolve(document, index))
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissionEntry {
    #[serde(default, deserialize_with = "deserialize_present")]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    feature: Option<RawAgentFeature>,
    permission: RawPermission,
}

impl RawPermissionEntry {
    fn resolve(
        self,
        document: usize,
        index: usize,
    ) -> Result<ResolvedPermissionEntry, AgentDefinitionError> {
        resolve_permission_entry(
            document,
            &format!("spec.tools[{index}]"),
            self.name,
            self.feature,
            self.permission,
        )
    }
}

fn resolve_permission_entry(
    document: usize,
    field: &str,
    name: Option<String>,
    feature: Option<RawAgentFeature>,
    permission: RawPermission,
) -> Result<ResolvedPermissionEntry, AgentDefinitionError> {
    let selector = match (name, feature) {
        (Some(name), None) => {
            AgentPermissionSelector::Name(non_empty(document, format!("{field}.name"), name)?)
        }
        (None, Some(RawAgentFeature::Agents)) => {
            return Err(invalid_value(
                document,
                format!("{field}.feature"),
                "`agents` owns no tools and cannot be used as a permission selector",
            ));
        }
        (None, Some(feature)) => AgentPermissionSelector::Feature(feature.resolve()),
        (Some(_), Some(_)) | (None, None) => {
            return Err(invalid_value(
                document,
                field,
                "exactly one of `name` or `feature` is required",
            ));
        }
    };
    Ok(ResolvedPermissionEntry {
        selector,
        permission: permission.resolve(document, &format!("{field}.permission"))?,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum RawAgentFeature {
    Filesystem,
    Agents,
    Skills,
    Shell,
    Task,
    Mcp,
}

impl RawAgentFeature {
    const fn resolve(self) -> AgentFeature {
        match self {
            Self::Filesystem => AgentFeature::Filesystem,
            Self::Agents => AgentFeature::Agents,
            Self::Skills => AgentFeature::Skills,
            Self::Shell => AgentFeature::Shell,
            Self::Task => AgentFeature::Task,
            Self::Mcp => AgentFeature::Mcp,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawPermission {
    Effect(RawEffect),
    Structured(RawStructuredPermission),
}

impl RawPermission {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<ResolvedPermission, AgentDefinitionError> {
        match self {
            Self::Effect(effect) => Ok(ResolvedPermission::Tool(effect.resolve())),
            Self::Structured(permission) => permission.resolve(document, field),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStructuredPermission {
    #[serde(default, deserialize_with = "deserialize_present")]
    resources: Option<RawPermissionResources>,
    #[serde(default, deserialize_with = "deserialize_present")]
    inputs: Option<Vec<RawInputRule>>,
}

impl RawStructuredPermission {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<ResolvedPermission, AgentDefinitionError> {
        match (self.resources, self.inputs) {
            (Some(resources), None) => resources
                .resolve(document, &format!("{field}.resources"))
                .map(ResolvedPermission::Resources),
            (None, Some(inputs)) if !inputs.is_empty() => inputs
                .into_iter()
                .enumerate()
                .map(|(index, rule)| rule.resolve(document, &format!("{field}.inputs[{index}]")))
                .collect::<Result<_, _>>()
                .map(ResolvedPermission::Inputs),
            (None, Some(_)) => Err(invalid_value(
                document,
                format!("{field}.inputs"),
                "at least one input rule is required",
            )),
            (Some(_), Some(_)) | (None, None) => Err(invalid_value(
                document,
                field,
                "exactly one of `resources` or `inputs` is required",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputRule {
    #[serde(rename = "match")]
    conditions: Vec<RawInputCondition>,
    effect: RawEffect,
}

impl RawInputRule {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<PermissionInputRule, AgentDefinitionError> {
        if self.conditions.is_empty() {
            return Err(invalid_value(
                document,
                format!("{field}.match"),
                "at least one input condition is required",
            ));
        }
        let conditions = self
            .conditions
            .into_iter()
            .enumerate()
            .map(|(index, condition)| {
                condition.resolve(document, &format!("{field}.match[{index}]"))
            })
            .collect::<Result<_, _>>()?;
        PermissionInputRule::new(conditions, self.effect.resolve())
            .map_err(|message| invalid_value(document, field, message))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputCondition {
    pointer: String,
    value: RawInputValue,
}

impl RawInputCondition {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<PermissionInputCondition, AgentDefinitionError> {
        let pointer = PermissionJsonPointer::parse(self.pointer)
            .map_err(|message| invalid_value(document, format!("{field}.pointer"), message))?;
        Ok(PermissionInputCondition {
            pointer,
            value: self.value.resolve(document, &format!("{field}.value"))?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputValue {
    #[serde(default, deserialize_with = "deserialize_present")]
    exact: Option<serde_yaml_ng::Value>,
    #[serde(default, deserialize_with = "deserialize_present")]
    wildcard: Option<String>,
}

impl RawInputValue {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<PermissionInputValueMatcher, AgentDefinitionError> {
        match (self.exact, self.wildcard) {
            (Some(value), None) => {
                let scalar = match value {
                    serde_yaml_ng::Value::Null => Ok(PermissionInputScalar::Null),
                    serde_yaml_ng::Value::Bool(value) => Ok(PermissionInputScalar::Bool(value)),
                    serde_yaml_ng::Value::Number(value) => {
                        PermissionInputScalar::number(&value.to_string())
                    }
                    serde_yaml_ng::Value::String(value) => Ok(PermissionInputScalar::String(value)),
                    serde_yaml_ng::Value::Sequence(_) | serde_yaml_ng::Value::Mapping(_) => {
                        Err("exact input value must be a JSON scalar".to_owned())
                    }
                    serde_yaml_ng::Value::Tagged(_) => {
                        Err("tagged exact input values are not supported".to_owned())
                    }
                };
                scalar
                    .map(PermissionInputValueMatcher::Exact)
                    .map_err(|message| invalid_value(document, format!("{field}.exact"), message))
            }
            (None, Some(wildcard)) => {
                if wildcard.matches('*').count() != 1 || !wildcard.ends_with('*') {
                    return Err(invalid_value(
                        document,
                        format!("{field}.wildcard"),
                        "wildcard must contain exactly one terminal `*`",
                    ));
                }
                Ok(PermissionInputValueMatcher::WildcardPrefix(
                    wildcard[..wildcard.len() - 1].to_owned(),
                ))
            }
            (Some(_), Some(_)) | (None, None) => Err(invalid_value(
                document,
                field,
                "exactly one of `exact` or `wildcard` is required",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissionResources {
    #[serde(default)]
    paths: Vec<RawPathRule>,
    #[serde(default)]
    commands: Vec<RawCommandRule>,
}

impl RawPermissionResources {
    fn resolve(
        self,
        document: usize,
        field: &str,
    ) -> Result<ResolvedPermissionResources, AgentDefinitionError> {
        if self.paths.is_empty() && self.commands.is_empty() {
            return Err(invalid_value(
                document,
                field,
                "at least one path or command rule is required",
            ));
        }
        let paths = self
            .paths
            .into_iter()
            .enumerate()
            .map(|(rule_index, rule)| rule.resolve(document, field, rule_index))
            .collect::<Result<_, _>>()?;
        let commands = self
            .commands
            .into_iter()
            .enumerate()
            .map(|(rule_index, rule)| rule.resolve(document, field, rule_index))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(ResolvedPermissionResources { paths, commands })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPathRule {
    #[serde(rename = "match")]
    matcher: RawPathMatch,
    effect: RawEffect,
}

impl RawPathRule {
    fn resolve(
        self,
        document: usize,
        field: &str,
        rule_index: usize,
    ) -> Result<PermissionDirectoryRule, AgentDefinitionError> {
        let root = workspace_root(
            document,
            format!("{field}.paths[{rule_index}].match.outsideRoot"),
            self.matcher.outside_root,
        )?;
        Ok(PermissionDirectoryRule {
            pattern: PermissionPathPattern::OutsideRoot(PathBuf::from(root)),
            effect: self.effect.resolve(),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPathMatch {
    outside_root: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandRule {
    #[serde(rename = "match")]
    matcher: RawCommandMatch,
    effect: RawEffect,
}

impl RawCommandRule {
    fn resolve(
        self,
        document: usize,
        field: &str,
        rule_index: usize,
    ) -> Result<Vec<PermissionCommandRule>, AgentDefinitionError> {
        if self.matcher.names.is_empty() {
            return Err(invalid_value(
                document,
                format!("{field}.commands[{rule_index}].match.names"),
                "at least one command name is required",
            ));
        }
        self.matcher
            .names
            .into_iter()
            .enumerate()
            .map(|(name_index, name)| {
                let name = non_empty(
                    document,
                    format!("{field}.commands[{rule_index}].match.names[{name_index}]"),
                    name,
                )?;
                Ok(PermissionCommandRule {
                    pattern: PermissionCommandPattern::Name(name),
                    effect: self.effect.resolve(),
                })
            })
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandMatch {
    names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawEffect {
    Allow,
    Ask,
    Deny,
}

impl RawEffect {
    const fn resolve(&self) -> PermissionEffect {
        match self {
            Self::Allow => PermissionEffect::allow(),
            Self::Ask => PermissionEffect::ask(),
            Self::Deny => PermissionEffect::deny(),
        }
    }
}

fn validate_api_version(document: usize, api_version: String) -> Result<(), AgentDefinitionError> {
    if api_version == AGENT_DEFINITION_API_VERSION {
        Ok(())
    } else {
        Err(AgentDefinitionError::UnsupportedApiVersion {
            document,
            api_version,
        })
    }
}

fn workspace_root(
    document: usize,
    field: impl Into<String>,
    root: String,
) -> Result<String, AgentDefinitionError> {
    if root == SESSION_WORKSPACE_ROOT {
        Ok(root)
    } else {
        Err(invalid_value(
            document,
            field,
            format!("only `{SESSION_WORKSPACE_ROOT}` is supported"),
        ))
    }
}

fn non_empty(
    document: usize,
    field: impl Into<String>,
    value: String,
) -> Result<String, AgentDefinitionError> {
    if value.trim().is_empty() {
        Err(invalid_value(document, field, "must not be empty"))
    } else {
        Ok(value)
    }
}

fn invalid_value(
    document: usize,
    field: impl Into<String>,
    message: impl Into<String>,
) -> AgentDefinitionError {
    AgentDefinitionError::InvalidValue {
        document,
        field: field.into(),
        message: message.into(),
    }
}

const fn default_true() -> bool {
    true
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
