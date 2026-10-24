//! Strict server-owned Routine definitions and immutable materialized entries.

use jaq_interpret::{Ctx, FilterT as _, ParseCtx, RcIter, Val};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::error::AgentDefinitionLoadError;

#[path = "routine/interaction.rs"]
mod interaction;
#[path = "routine/interaction_limits.rs"]
mod interaction_limits;
#[cfg(test)]
#[path = "routine/interaction_tests.rs"]
mod interaction_tests;
pub use interaction::*;
pub use interaction_limits::InteractionLimits;

pub(super) const MAX_ROUTINE_SOURCE_BYTES: usize = 256 * 1024;
pub(super) const MAX_ROUTINE_COUNT: usize = 256;
const MAX_ROUTINE_TRIGGERS: usize = 32;
const MAX_ROUTINE_PARAMS: usize = 64;
const MAX_ROUTINE_TASKS: usize = 64;
const MAX_ROUTINE_RESULTS: usize = 64;
const MAX_ROUTINE_CONDITIONS: usize = 32;
const MAX_ROUTINE_CONDITION_DEPTH: usize = 8;
const MAX_ROUTINE_VALUES: usize = 32;
const MAX_ROUTINE_LIST_ITEMS: usize = 128;
const MAX_ROUTINE_MUTATIONS: usize = 1;
const MAX_ROUTINE_STRING_BYTES: usize = 16 * 1024;
const MAX_ROUTINE_EXPRESSION_BYTES: usize = 1024;
const MAX_ROUTINE_PATTERN_BYTES: usize = 1024;
const MAX_ROUTINE_MAX_TURNS: u32 = 128;

/// One immutable, validated routine in a catalog snapshot.
#[derive(Debug, Clone)]
pub struct MaterializedRoutine {
    name: String,
    source_path: String,
    source_etag: String,
    definition: Arc<RoutineDefinition>,
}

impl MaterializedRoutine {
    pub(super) fn new(
        name: String,
        source_path: String,
        source_etag: String,
        definition: RoutineDefinition,
    ) -> Self {
        Self {
            name,
            source_path,
            source_etag,
            definition: Arc::new(definition),
        }
    }

    /// Return the canonical routine name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the canonical catalog source path.
    #[must_use]
    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    /// Return the exact active source `ETag`.
    #[must_use]
    pub fn source_etag(&self) -> &str {
        &self.source_etag
    }

    /// Return the immutable typed definition.
    #[must_use]
    pub fn definition(&self) -> &RoutineDefinition {
        &self.definition
    }
}

/// Strict Routine resource document.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineDefinition {
    /// Version of the Routine schema.
    pub api_version: String,
    /// Resource kind, always `Routine` after validation.
    pub kind: String,
    /// Canonical resource identity.
    pub metadata: RoutineMetadata,
    /// Executable routine specification.
    pub spec: RoutineSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineMetadata {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSpec {
    pub description: String,
    #[serde(default)]
    pub params: Vec<RoutineParam>,
    pub triggers: Vec<RoutineTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<RoutineSourceSession>,
    pub tasks: Vec<RoutineTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub work_groups: Vec<RoutineWorkGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<RoutineStreamOutput>,
    #[serde(default)]
    pub results: Vec<RoutineResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineParam {
    pub name: String,
    pub r#type: RoutineResultType,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<RoutineScalar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<RoutineParamValidation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineParamValidation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineTrigger {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<RoutineEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<RoutineManualTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_lifecycle: Option<RoutineSessionLifecycleTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction: Option<RoutineInteractionTrigger>,
    #[serde(default)]
    pub when: Vec<RoutineCondition>,
    #[serde(default)]
    pub params: Vec<RoutineTriggerParam>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineManualTrigger {
    pub display_name: String,
    pub scope: RoutineManualScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_busy: Option<RoutineManualOnBusy>,
    pub inputs: Vec<RoutineManualInput>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineManualOnBusy {
    Reject,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineManualScope {
    Session,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineManualInput {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionLifecycleTrigger {
    pub event: RoutineSessionLifecycleEvent,
    pub scope: RoutineManualScope,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineSessionLifecycleEvent {
    RunCompleted,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineTriggerParam {
    pub name: String,
    pub value: RoutineScalar,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineSourceSession {
    pub from: RoutineScalar,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<RoutineSourceSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<RoutineSourcePause>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSourceSnapshot {
    pub through_cursor: RoutineScalar,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineSourcePause {
    pub submissions: RoutinePausedSubmissions,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutinePausedSubmissions {
    Queue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineEvent {
    pub r#type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RoutineCondition {
    Input(RoutineInputCondition),
    Metadata(RoutineMetadataConditionEnvelope),
    Any(RoutineAnyCondition),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineAnyCondition {
    pub any: Vec<RoutineCondition>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineInputCondition {
    pub input: RoutineScalar,
    pub operator: RoutineConditionOperator,
    pub values: Vec<RoutineScalar>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineMetadataConditionEnvelope {
    pub metadata: RoutineMetadataCondition,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineMetadataCondition {
    pub key: String,
    pub operator: RoutineConditionOperator,
    pub values: Vec<RoutineScalar>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum RoutineConditionOperator {
    In,
    NotIn,
    GreaterThanOrEqual,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RoutineScalar {
    String(String),
    Boolean(bool),
    Integer(u64),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineTask {
    pub name: String,
    #[serde(default)]
    pub run_after: Vec<String>,
    #[serde(default)]
    pub when: Vec<RoutineCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_group: Option<String>,
    pub task_spec: RoutineTaskSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineTaskSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcribe: Option<RoutineTranscribe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesize: Option<RoutineSynthesize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect: Option<RoutineCollect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_template: Option<RoutineSessionTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RoutineAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<RoutineLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<RoutineOutput>,
    #[serde(default)]
    pub results: Vec<RoutineResult>,
}

impl RoutineTaskSpec {
    pub(crate) fn requires_interaction(&self) -> bool {
        self.transcribe.is_some() || self.synthesize.is_some() || self.collect.is_some()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionTemplate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<RoutineModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RoutineSessionTemplateSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference: Option<RoutineInference>,
    pub system_prompt: RoutinePrompt,
    pub user_prompt: RoutinePrompt,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionTemplateSource {
    pub session: RoutineScalar,
    pub through_cursor: RoutineScalar,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineInference {
    pub inherit_from: RoutineScalar,
    pub reasoning: RoutineReasoning,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutineReasoning {
    Disabled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutinePrompt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RoutinePromptMode>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutinePromptMode {
    Replace,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineLimits {
    #[serde(rename = "maxTurns")]
    pub max_turns: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineOutput {
    pub r#type: RoutineOutputType,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutineOutputType {
    Text,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineAction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_metadata: Option<RoutineSessionMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_create: Option<RoutineSessionCreate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_mask: Option<RoutineSessionMask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_continue: Option<RoutineSessionContinue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionMask {
    pub session_id: RoutineScalar,
    pub atomic: bool,
    pub masks: RoutineSessionMasks,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineSessionMasks {
    pub subtract: Vec<RoutineMaskSubtraction>,
    pub add: Vec<RoutineReplacementMask>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineMaskSubtraction {
    pub selector: RoutineMaskOverlapSelector,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineMaskOverlapSelector {
    pub overlaps: RoutineMaskThroughCursor,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineMaskThroughCursor {
    pub through_cursor: RoutineScalar,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineReplacementMask {
    pub r#type: RoutineMaskType,
    pub key: String,
    pub selector: RoutineMaskThroughCursor,
    pub on_overlap: RoutineMaskOverlapPolicy,
    pub preserve_source_items: bool,
    pub replacement: RoutineMaskReplacement,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineMaskType {
    Replacement,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineMaskOverlapPolicy {
    Reject,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineMaskReplacement {
    pub role: RoutineMaskReplacementRole,
    pub text: RoutineScalar,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineMaskReplacementRole {
    User,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionContinue {
    pub session_id: RoutineScalar,
    pub from_cursor: RoutineScalar,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionCreate {
    pub inherit_from: RoutineSessionCreateInheritance,
    pub metadata: BTreeMap<String, RoutineScalar>,
    pub first_user_message: RoutineFirstUserMessage,
    pub start: RoutineSessionStart,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionCreateInheritance {
    pub session: RoutineScalar,
    pub inference: RoutineExactInheritance,
    pub agent_definition: RoutineExactInheritance,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineExactInheritance {
    Exact,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineFirstUserMessage {
    pub text: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineSessionStart {
    Immediate,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineSessionMetadata {
    pub session_id: RoutineScalar,
    pub mutations: Vec<RoutineMetadataMutation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineMetadataMutation {
    pub operation: RoutineMetadataOperation,
    pub key: String,
    pub value: RoutineScalar,
    pub if_absent: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutineMetadataOperation {
    Set,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineResult {
    pub name: String,
    pub r#type: RoutineResultType,
    pub value: RoutineScalar,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineResultType {
    String,
    Boolean,
}

pub(super) fn materialize_routine(
    source: &str,
    path: &str,
) -> Result<RoutineDefinition, AgentDefinitionLoadError> {
    if source.len() > MAX_ROUTINE_SOURCE_BYTES {
        return Err(invalid(
            path,
            format!("source exceeds {MAX_ROUTINE_SOURCE_BYTES} bytes"),
        ));
    }
    let mut definition: RoutineDefinition = serde_yaml_ng::from_str(source).map_err(|source| {
        AgentDefinitionLoadError::ResourceYaml {
            path: path.to_owned(),
            source,
        }
    })?;
    for task in &mut definition.spec.tasks {
        if task.task_spec.session_template.is_some() {
            task.task_spec.features.get_or_insert_with(Vec::new);
            task.task_spec.permissions.get_or_insert_with(Vec::new);
        }
    }
    validate_routine(&definition, path)?;
    Ok(definition)
}

#[allow(
    clippy::too_many_lines,
    reason = "validation reports exact schema paths inline"
)]
fn validate_routine(
    definition: &RoutineDefinition,
    path: &str,
) -> Result<(), AgentDefinitionLoadError> {
    if definition.api_version != waltr_agent::AGENT_DEFINITION_API_VERSION
        || definition.kind != "Routine"
    {
        return Err(invalid(
            path,
            "expected apiVersion `waltr.dev/v1alpha1` and kind `Routine`".to_owned(),
        ));
    }
    validate_string(path, "spec.description", &definition.spec.description)?;
    bounded_count(
        path,
        "spec.params",
        definition.spec.params.len(),
        MAX_ROUTINE_PARAMS,
        true,
    )?;
    bounded_count(
        path,
        "spec.triggers",
        definition.spec.triggers.len(),
        MAX_ROUTINE_TRIGGERS,
        false,
    )?;
    bounded_count(
        path,
        "spec.tasks",
        definition.spec.tasks.len(),
        MAX_ROUTINE_TASKS,
        false,
    )?;
    bounded_count(
        path,
        "spec.results",
        definition.spec.results.len(),
        MAX_ROUTINE_RESULTS,
        true,
    )?;

    let mut params = BTreeMap::new();
    for (index, param) in definition.spec.params.iter().enumerate() {
        let location = format!("spec.params[{index}]");
        insert_unique(path, &location, &param.name, &mut params)?;
        if param.required && param.default.is_some() {
            return Err(invalid(
                path,
                format!("{location} cannot be required and declare a default"),
            ));
        }
        match (param.r#type, &param.validation) {
            (RoutineResultType::Boolean, Some(_)) => {
                return Err(invalid(
                    path,
                    format!("{location}.validation is supported only for string parameters"),
                ));
            }
            (RoutineResultType::String, Some(validation)) => {
                if validation
                    .min_length
                    .is_some_and(|minimum| minimum > MAX_ROUTINE_STRING_BYTES)
                {
                    return Err(invalid(
                        path,
                        format!(
                            "{location}.validation.minLength exceeds {MAX_ROUTINE_STRING_BYTES}"
                        ),
                    ));
                }
                if let Some(pattern) = &validation.pattern {
                    validate_pattern(path, &format!("{location}.validation.pattern"), pattern)?;
                }
            }
            (_, None) => {}
        }
        if let Some(default) = &param.default {
            validate_parameter_value(path, &format!("{location}.default"), param, default, false)?;
        }
    }

    let mut trigger_names = BTreeMap::new();
    let required = definition
        .spec
        .params
        .iter()
        .filter(|param| param.required)
        .map(|param| param.name.as_str())
        .collect::<BTreeSet<_>>();
    for (index, trigger) in definition.spec.triggers.iter().enumerate() {
        let location = format!("spec.triggers[{index}]");
        insert_unique(path, &location, &trigger.name, &mut trigger_names)?;
        match (
            &trigger.event,
            &trigger.manual,
            &trigger.session_lifecycle,
            &trigger.interaction,
        ) {
            (Some(event), None, None, None) => {
                if definition.spec.source_session.is_some() {
                    return Err(invalid(
                        path,
                        format!(
                            "{location}.event cannot be combined with spec.sourceSession; source capture requires a manual trigger"
                        ),
                    ));
                }
                validate_string(path, &format!("{location}.event.type"), &event.r#type)?;
                if event.r#type.contains('*') {
                    return Err(invalid(
                        path,
                        format!("{location}.event.type must be an exact event type"),
                    ));
                }
            }
            (None, Some(manual), None, None) => {
                if definition
                    .spec
                    .source_session
                    .as_ref()
                    .is_some_and(|source| source.pause.is_some())
                    && manual.on_busy.is_none()
                {
                    return Err(invalid(
                        path,
                        format!("{location}.manual.onBusy must be `reject` for a paused source"),
                    ));
                }
                validate_string(
                    path,
                    &format!("{location}.manual.displayName"),
                    &manual.display_name,
                )?;
                bounded_count(
                    path,
                    &format!("{location}.manual.inputs"),
                    manual.inputs.len(),
                    MAX_ROUTINE_PARAMS,
                    true,
                )?;
                let mut inputs = BTreeMap::new();
                for (input_index, input) in manual.inputs.iter().enumerate() {
                    let input_location = format!("{location}.manual.inputs[{input_index}]");
                    insert_unique(path, &input_location, &input.name, &mut inputs)?;
                    if !params.contains_key(input.name.as_str()) {
                        return Err(invalid(
                            path,
                            format!(
                                "{input_location}.name references undeclared parameter `{}`",
                                input.name
                            ),
                        ));
                    }
                }
                let exposed = inputs.keys().copied().collect::<BTreeSet<_>>();
                if let Some(hidden) = required.difference(&exposed).next() {
                    return Err(invalid(
                        path,
                        format!("{location}.manual.inputs hides required parameter `{hidden}`"),
                    ));
                }
            }
            (None, None, Some(_), None) => {}
            (None, None, None, Some(interaction)) => {
                validate_interaction_trigger(path, interaction, definition)?;
            }
            _ => {
                return Err(invalid(
                    path,
                    format!(
                        "{location} must declare exactly one of event, manual, sessionLifecycle, or interaction"
                    ),
                ));
            }
        }
        bounded_count(
            path,
            &format!("{location}.params"),
            trigger.params.len(),
            MAX_ROUTINE_PARAMS,
            true,
        )?;
        let mut assignments = BTreeMap::new();
        for (param_index, assignment) in trigger.params.iter().enumerate() {
            let assignment_location = format!("{location}.params[{param_index}]");
            insert_unique(
                path,
                &assignment_location,
                &assignment.name,
                &mut assignments,
            )?;
            let parameter = params.get(assignment.name.as_str()).ok_or_else(|| {
                invalid(
                    path,
                    format!(
                        "{assignment_location}.name references undeclared parameter `{}`",
                        assignment.name
                    ),
                )
            })?;
            validate_parameter_value(
                path,
                &format!("{assignment_location}.value"),
                &definition.spec.params[*parameter],
                &assignment.value,
                true,
            )?;
        }
        if trigger.session_lifecycle.is_none() && !trigger.params.is_empty() {
            return Err(invalid(
                path,
                format!("{location}.params is supported only for sessionLifecycle triggers"),
            ));
        }
        if trigger.session_lifecycle.is_some()
            && let Some(missing) = required
                .iter()
                .find(|name| !assignments.contains_key(**name))
        {
            return Err(invalid(
                path,
                format!("{location}.params does not assign required parameter `{missing}`"),
            ));
        }
        validate_conditions(path, &location, &trigger.when, trigger.event.is_some())?;
    }

    if let Some(source) = &definition.spec.source_session {
        validate_expression_scalar(path, "spec.sourceSession.from", &source.from)?;
        match (&source.snapshot, &source.pause) {
            (Some(snapshot), None) => validate_expression_scalar(
                path,
                "spec.sourceSession.snapshot.throughCursor",
                &snapshot.through_cursor,
            )?,
            (None, Some(_)) => {}
            _ => {
                return Err(invalid(
                    path,
                    "spec.sourceSession must declare exactly one of snapshot or pause".to_owned(),
                ));
            }
        }
    }

    let mut tasks = BTreeMap::new();
    for (index, task) in definition.spec.tasks.iter().enumerate() {
        insert_unique(
            path,
            &format!("spec.tasks[{index}]"),
            &task.name,
            &mut tasks,
        )?;
    }
    for (index, task) in definition.spec.tasks.iter().enumerate() {
        validate_task(path, index, task, &tasks)?;
        if task.task_spec.requires_interaction()
            && definition
                .spec
                .triggers
                .iter()
                .any(|trigger| trigger.interaction.is_none())
        {
            return Err(invalid(
                path,
                "speech and collector tasks require interaction triggers".to_owned(),
            ));
        }
    }
    validate_dag(path, &definition.spec.tasks, &tasks)?;
    validate_interaction_graph(path, definition)?;

    let task_results = definition
        .spec
        .tasks
        .iter()
        .map(|task| {
            (
                task.name.as_str(),
                task.task_spec
                    .results
                    .iter()
                    .map(|result| (result.name.as_str(), result.r#type))
                    .collect::<BTreeMap<_, _>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    validate_static_references(path, definition, &task_results)?;
    validate_results(
        path,
        "spec.results",
        &definition.spec.results,
        &task_results,
        definition,
        None,
        false,
        None,
    )?;
    for (index, task) in definition.spec.tasks.iter().enumerate() {
        let ancestors = transitive_ancestors(task, &definition.spec.tasks);
        let action_kind = task.task_spec.action.as_ref().map(|action| {
            if action.session_metadata.is_some() {
                ActionKind::SessionMetadata
            } else if action.session_create.is_some() {
                ActionKind::SessionCreate
            } else if action.session_mask.is_some() {
                ActionKind::SessionMask
            } else {
                ActionKind::SessionContinue
            }
        });
        validate_results(
            path,
            &format!("spec.tasks[{index}].taskSpec.results"),
            &task.task_spec.results,
            &task_results,
            definition,
            Some(&ancestors),
            task.task_spec.session_template.is_some() || task.task_spec.collect.is_some(),
            action_kind,
        )?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "validation reports exact task schema paths inline"
)]
fn validate_task(
    path: &str,
    index: usize,
    task: &RoutineTask,
    tasks: &BTreeMap<&str, usize>,
) -> Result<(), AgentDefinitionLoadError> {
    let location = format!("spec.tasks[{index}]");
    bounded_count(
        path,
        &format!("{location}.runAfter"),
        task.run_after.len(),
        MAX_ROUTINE_TASKS,
        true,
    )?;
    validate_conditions(path, &location, &task.when, false)?;
    for (dependency_index, dependency) in task.run_after.iter().enumerate() {
        validate_string(
            path,
            &format!("{location}.runAfter[{dependency_index}]"),
            dependency,
        )?;
        if !tasks.contains_key(dependency.as_str()) {
            return Err(invalid(
                path,
                format!(
                    "{location}.runAfter[{dependency_index}] references unknown task `{dependency}`"
                ),
            ));
        }
    }

    let spec = &task.task_spec;
    let kinds = [
        spec.session_template.is_some(),
        spec.action.is_some(),
        spec.transcribe.is_some(),
        spec.synthesize.is_some(),
        spec.collect.is_some(),
    ];
    if kinds.into_iter().filter(|present| *present).count() != 1 {
        return Err(invalid(
            path,
            format!("{location}.taskSpec requires exactly one task kind"),
        ));
    }
    if spec.transcribe.is_some() || spec.synthesize.is_some() || spec.collect.is_some() {
        return validate_stream_task(path, task);
    }
    match (&spec.session_template, &spec.action) {
        (Some(session), None) => {
            let features = spec.features.as_ref().ok_or_else(|| {
                invalid(
                    path,
                    format!("{location}.taskSpec.features is required for agent tasks"),
                )
            })?;
            let tools = spec.tools.as_ref().ok_or_else(|| {
                invalid(
                    path,
                    format!("{location}.taskSpec.tools is required for agent tasks"),
                )
            })?;
            let permissions = spec.permissions.as_ref().ok_or_else(|| {
                invalid(
                    path,
                    format!("{location}.taskSpec.permissions is required for agent tasks"),
                )
            })?;
            validate_string_list(path, &format!("{location}.taskSpec.features"), features)?;
            validate_string_list(path, &format!("{location}.taskSpec.tools"), tools)?;
            validate_string_list(
                path,
                &format!("{location}.taskSpec.permissions"),
                permissions,
            )?;
            let limits = spec.limits.as_ref().ok_or_else(|| {
                invalid(
                    path,
                    format!("{location}.taskSpec.limits is required for agent tasks"),
                )
            })?;
            if limits.max_turns == 0 || limits.max_turns > MAX_ROUTINE_MAX_TURNS {
                return Err(invalid(
                    path,
                    format!(
                        "{location}.taskSpec.limits.maxTurns must be between 1 and {MAX_ROUTINE_MAX_TURNS}"
                    ),
                ));
            }
            if spec.output.is_none() {
                return Err(invalid(
                    path,
                    format!("{location}.taskSpec.output is required for agent tasks"),
                ));
            }
            if let Some(model) = &session.model {
                validate_model_ref(path, model)?;
                if session.source.is_some() || session.inference.is_some() {
                    return Err(invalid(
                        path,
                        format!(
                            "{location}.sessionTemplate.model cannot be combined with source or inference"
                        ),
                    ));
                }
            }
            if session.source.is_none() && session.inference.is_none() && session.model.is_none() {
                return Err(invalid(
                    path,
                    format!("{location}.taskSpec.sessionTemplate requires source or inference"),
                ));
            }
            if let Some(source) = &session.source {
                validate_expression_scalar(
                    path,
                    &format!("{location}.taskSpec.sessionTemplate.source.session"),
                    &source.session,
                )?;
                validate_expression_scalar(
                    path,
                    &format!("{location}.taskSpec.sessionTemplate.source.throughCursor"),
                    &source.through_cursor,
                )?;
            }
            if let Some(inference) = &session.inference {
                validate_expression_scalar(
                    path,
                    &format!("{location}.taskSpec.sessionTemplate.inference.inheritFrom"),
                    &inference.inherit_from,
                )?;
            }
            if session.system_prompt.mode.is_none() {
                return Err(invalid(
                    path,
                    format!(
                        "{location}.taskSpec.sessionTemplate.systemPrompt.mode must be `replace`"
                    ),
                ));
            }
            validate_prompt(
                path,
                &format!("{location}.taskSpec.sessionTemplate.systemPrompt.text"),
                &session.system_prompt.text,
            )?;
            validate_prompt(
                path,
                &format!("{location}.taskSpec.sessionTemplate.userPrompt.text"),
                &session.user_prompt.text,
            )?;
        }
        (None, Some(action)) => {
            if spec.features.is_some()
                || spec.tools.is_some()
                || spec.permissions.is_some()
                || spec.limits.is_some()
                || spec.output.is_some()
            {
                return Err(invalid(
                    path,
                    format!("{location}.taskSpec action tasks cannot declare agent task fields"),
                ));
            }
            match (
                &action.session_metadata,
                &action.session_create,
                &action.session_mask,
                &action.session_continue,
            ) {
                (Some(metadata), None, None, None) => {
                    validate_expression_scalar(
                        path,
                        &format!("{location}.taskSpec.action.sessionMetadata.sessionId"),
                        &metadata.session_id,
                    )?;
                    bounded_count(
                        path,
                        &format!("{location}.taskSpec.action.sessionMetadata.mutations"),
                        metadata.mutations.len(),
                        MAX_ROUTINE_MUTATIONS,
                        false,
                    )?;
                    for (mutation_index, mutation) in metadata.mutations.iter().enumerate() {
                        let mutation_path = format!(
                            "{location}.taskSpec.action.sessionMetadata.mutations[{mutation_index}]"
                        );
                        validate_string(path, &format!("{mutation_path}.key"), &mutation.key)?;
                        validate_expression_scalar(
                            path,
                            &format!("{mutation_path}.value"),
                            &mutation.value,
                        )?;
                        if !mutation.if_absent {
                            return Err(invalid(
                                path,
                                format!("{mutation_path}.ifAbsent must be true"),
                            ));
                        }
                    }
                }
                (None, Some(create), None, None) => {
                    validate_expression_scalar(
                        path,
                        &format!("{location}.taskSpec.action.sessionCreate.inheritFrom.session"),
                        &create.inherit_from.session,
                    )?;
                    bounded_count(
                        path,
                        &format!("{location}.taskSpec.action.sessionCreate.metadata"),
                        create.metadata.len(),
                        MAX_ROUTINE_VALUES,
                        true,
                    )?;
                    for (key, value) in &create.metadata {
                        validate_string(
                            path,
                            &format!("{location}.taskSpec.action.sessionCreate.metadata.{key}"),
                            key,
                        )?;
                        validate_scalar(
                            path,
                            &format!("{location}.taskSpec.action.sessionCreate.metadata.{key}"),
                            value,
                            true,
                        )?;
                    }
                    validate_prompt(
                        path,
                        &format!("{location}.taskSpec.action.sessionCreate.firstUserMessage.text"),
                        &create.first_user_message.text,
                    )?;
                }
                (None, None, Some(mask), None) => {
                    let base = format!("{location}.taskSpec.action.sessionMask");
                    validate_expression_scalar(
                        path,
                        &format!("{base}.sessionId"),
                        &mask.session_id,
                    )?;
                    if !mask.atomic {
                        return Err(invalid(path, format!("{base}.atomic must be true")));
                    }
                    bounded_count(
                        path,
                        &format!("{base}.masks.subtract"),
                        mask.masks.subtract.len(),
                        1,
                        false,
                    )?;
                    bounded_count(
                        path,
                        &format!("{base}.masks.add"),
                        mask.masks.add.len(),
                        1,
                        false,
                    )?;
                    validate_expression_scalar(
                        path,
                        &format!("{base}.masks.subtract[0].selector.overlaps.throughCursor"),
                        &mask.masks.subtract[0].selector.overlaps.through_cursor,
                    )?;
                    let addition = &mask.masks.add[0];
                    validate_string(path, &format!("{base}.masks.add[0].key"), &addition.key)?;
                    validate_expression_scalar(
                        path,
                        &format!("{base}.masks.add[0].selector.throughCursor"),
                        &addition.selector.through_cursor,
                    )?;
                    if !addition.preserve_source_items {
                        return Err(invalid(
                            path,
                            format!("{base}.masks.add[0].preserveSourceItems must be true"),
                        ));
                    }
                    validate_expression_scalar(
                        path,
                        &format!("{base}.masks.add[0].replacement.text"),
                        &addition.replacement.text,
                    )?;
                }
                (None, None, None, Some(continuation)) => {
                    validate_expression_scalar(
                        path,
                        &format!("{location}.taskSpec.action.sessionContinue.sessionId"),
                        &continuation.session_id,
                    )?;
                    validate_expression_scalar(
                        path,
                        &format!("{location}.taskSpec.action.sessionContinue.fromCursor"),
                        &continuation.from_cursor,
                    )?;
                    if !spec.results.is_empty() {
                        return Err(invalid(
                            path,
                            format!(
                                "{location}.taskSpec.action.sessionContinue has no result fields"
                            ),
                        ));
                    }
                }
                _ => {
                    return Err(invalid(
                        path,
                        format!(
                            "{location}.taskSpec.action must declare exactly one supported action"
                        ),
                    ));
                }
            }
        }
        _ => {
            return Err(invalid(
                path,
                format!(
                    "{location}.taskSpec must declare exactly one of sessionTemplate or action"
                ),
            ));
        }
    }
    bounded_count(
        path,
        &format!("{location}.taskSpec.results"),
        spec.results.len(),
        MAX_ROUTINE_RESULTS,
        true,
    )?;
    Ok(())
}

fn validate_conditions(
    path: &str,
    parent: &str,
    conditions: &[RoutineCondition],
    metadata_allowed: bool,
) -> Result<(), AgentDefinitionLoadError> {
    bounded_count(
        path,
        &format!("{parent}.when"),
        conditions.len(),
        MAX_ROUTINE_CONDITIONS,
        true,
    )?;
    for (index, condition) in conditions.iter().enumerate() {
        let location = format!("{parent}.when[{index}]");
        validate_condition(path, &location, condition, metadata_allowed, 1)?;
    }
    Ok(())
}

fn validate_condition(
    path: &str,
    location: &str,
    condition: &RoutineCondition,
    metadata_allowed: bool,
    depth: usize,
) -> Result<(), AgentDefinitionLoadError> {
    if depth > MAX_ROUTINE_CONDITION_DEPTH {
        return Err(invalid(
            path,
            format!("{location} exceeds condition nesting depth {MAX_ROUTINE_CONDITION_DEPTH}"),
        ));
    }
    let (operator, values, values_location) = match condition {
        RoutineCondition::Input(condition) => {
            validate_expression_scalar(path, &format!("{location}.input"), &condition.input)?;
            (
                condition.operator,
                &condition.values,
                format!("{location}.values"),
            )
        }
        RoutineCondition::Metadata(condition) => {
            if !metadata_allowed {
                return Err(invalid(
                    path,
                    format!("{location}.metadata is only supported on event trigger conditions"),
                ));
            }
            validate_string(
                path,
                &format!("{location}.metadata.key"),
                &condition.metadata.key,
            )?;
            (
                condition.metadata.operator,
                &condition.metadata.values,
                format!("{location}.metadata.values"),
            )
        }
        RoutineCondition::Any(condition) => {
            bounded_count(
                path,
                &format!("{location}.any"),
                condition.any.len(),
                MAX_ROUTINE_CONDITIONS,
                false,
            )?;
            for (index, child) in condition.any.iter().enumerate() {
                validate_condition(
                    path,
                    &format!("{location}.any[{index}]"),
                    child,
                    metadata_allowed,
                    depth + 1,
                )?;
            }
            return Ok(());
        }
    };
    bounded_count(
        path,
        &values_location,
        values.len(),
        if operator == RoutineConditionOperator::GreaterThanOrEqual {
            1
        } else {
            MAX_ROUTINE_VALUES
        },
        false,
    )?;
    for (value_index, value) in values.iter().enumerate() {
        let value_location = format!("{values_location}[{value_index}]");
        validate_scalar(path, &value_location, value, true)?;
        if operator == RoutineConditionOperator::GreaterThanOrEqual
            && !numeric_literal_or_expression(value)
        {
            return Err(invalid(
                path,
                format!(
                    "{value_location} must be a nonnegative integer or canonical decimal string"
                ),
            ));
        }
    }
    Ok(())
}

fn numeric_literal_or_expression(value: &RoutineScalar) -> bool {
    match value {
        RoutineScalar::Integer(_) => true,
        RoutineScalar::String(value) if value.starts_with("$(") => true,
        RoutineScalar::String(value) => canonical_decimal(value).is_some(),
        RoutineScalar::Boolean(_) => false,
    }
}

fn canonical_decimal(value: &str) -> Option<u64> {
    if value == "0" || (!value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit()))
    {
        value.parse().ok()
    } else {
        None
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "result validation requires its exact expression context"
)]
fn validate_results(
    path: &str,
    location: &str,
    results: &[RoutineResult],
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    allowed_tasks: Option<&BTreeSet<&str>>,
    output_allowed: bool,
    action_kind: Option<ActionKind>,
) -> Result<(), AgentDefinitionLoadError> {
    let mut names = BTreeMap::new();
    for (index, result) in results.iter().enumerate() {
        let result_path = format!("{location}[{index}]");
        insert_unique(path, &result_path, &result.name, &mut names)?;
        let expression = expression(path, &format!("{result_path}.value"), &result.value)?;
        validate_static_reference(
            path,
            &format!("{result_path}.value"),
            expression,
            task_results,
            definition,
            allowed_tasks,
            output_allowed,
            action_kind,
        )?;
        if let Some(reference) = expression.strip_prefix("tasks.") {
            let parts = reference.split('.').collect::<Vec<_>>();
            if parts.len() != 3 || parts[1] != "results" {
                return Err(invalid(
                    path,
                    format!("{result_path}.value has an unsupported task result reference"),
                ));
            }
            let referenced_type = task_results
                .get(parts[0])
                .and_then(|results| results.get(parts[2]))
                .ok_or_else(|| {
                    invalid(
                        path,
                        format!(
                            "{result_path}.value references unknown task result `{expression}`"
                        ),
                    )
                })?;
            if *referenced_type != result.r#type {
                return Err(invalid(
                    path,
                    format!("{result_path}.value result type does not match `{expression}`"),
                ));
            }
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "context-aware validation visits every expression-bearing schema location"
)]
fn validate_static_references(
    path: &str,
    definition: &RoutineDefinition,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
) -> Result<(), AgentDefinitionLoadError> {
    for (trigger_index, trigger) in definition.spec.triggers.iter().enumerate() {
        for (condition_index, condition) in trigger.when.iter().enumerate() {
            let location = format!("spec.triggers[{trigger_index}].when[{condition_index}]");
            validate_trigger_condition_references(
                path,
                &location,
                condition,
                task_results,
                definition,
                trigger,
            )?;
        }
        for (param_index, assignment) in trigger.params.iter().enumerate() {
            validate_trigger_value_references(
                path,
                &format!("spec.triggers[{trigger_index}].params[{param_index}].value"),
                &assignment.value,
                task_results,
                definition,
                trigger,
            )?;
        }
    }
    if let Some(source) = &definition.spec.source_session {
        validate_value_references(
            path,
            "spec.sourceSession.from",
            &source.from,
            task_results,
            definition,
            None,
            false,
            None,
        )?;
        if let Some(snapshot) = &source.snapshot {
            validate_value_references(
                path,
                "spec.sourceSession.snapshot.throughCursor",
                &snapshot.through_cursor,
                task_results,
                definition,
                None,
                false,
                None,
            )?;
        }
    }
    for (task_index, task) in definition.spec.tasks.iter().enumerate() {
        let location = format!("spec.tasks[{task_index}]");
        let ancestors = transitive_ancestors(task, &definition.spec.tasks);
        let action_kind = task.task_spec.action.as_ref().map(|action| {
            if action.session_metadata.is_some() {
                ActionKind::SessionMetadata
            } else if action.session_create.is_some() {
                ActionKind::SessionCreate
            } else if action.session_mask.is_some() {
                ActionKind::SessionMask
            } else {
                ActionKind::SessionContinue
            }
        });
        for (condition_index, condition) in task.when.iter().enumerate() {
            validate_task_condition_references(
                path,
                &format!("{location}.when[{condition_index}]"),
                condition,
                task_results,
                definition,
                &ancestors,
                action_kind,
            )?;
        }
        if let Some(session) = &task.task_spec.session_template {
            if let Some(source) = &session.source {
                validate_value_references(
                    path,
                    &format!("{location}.taskSpec.sessionTemplate.source.session"),
                    &source.session,
                    task_results,
                    definition,
                    Some(&ancestors),
                    false,
                    None,
                )?;
                validate_value_references(
                    path,
                    &format!("{location}.taskSpec.sessionTemplate.source.throughCursor"),
                    &source.through_cursor,
                    task_results,
                    definition,
                    Some(&ancestors),
                    false,
                    None,
                )?;
            }
            if let Some(inference) = &session.inference {
                validate_value_references(
                    path,
                    &format!("{location}.taskSpec.sessionTemplate.inference.inheritFrom"),
                    &inference.inherit_from,
                    task_results,
                    definition,
                    Some(&ancestors),
                    false,
                    None,
                )?;
            }
            validate_text_references(
                path,
                &format!("{location}.taskSpec.sessionTemplate.systemPrompt.text"),
                &session.system_prompt.text,
                task_results,
                definition,
                Some(&ancestors),
                false,
                None,
            )?;
            validate_text_references(
                path,
                &format!("{location}.taskSpec.sessionTemplate.userPrompt.text"),
                &session.user_prompt.text,
                task_results,
                definition,
                Some(&ancestors),
                false,
                None,
            )?;
        }
        if let Some(action) = &task.task_spec.action {
            if let Some(metadata) = &action.session_metadata {
                validate_value_references(
                    path,
                    &format!("{location}.taskSpec.action.sessionMetadata.sessionId"),
                    &metadata.session_id,
                    task_results,
                    definition,
                    Some(&ancestors),
                    false,
                    Some(ActionKind::SessionMetadata),
                )?;
                for (mutation_index, mutation) in metadata.mutations.iter().enumerate() {
                    validate_value_references(
                        path,
                        &format!(
                            "{location}.taskSpec.action.sessionMetadata.mutations[{mutation_index}].value"
                        ),
                        &mutation.value,
                        task_results,
                        definition,
                        Some(&ancestors),
                        false,
                        Some(ActionKind::SessionMetadata),
                    )?;
                }
            }
            if let Some(create) = &action.session_create {
                validate_value_references(
                    path,
                    &format!("{location}.taskSpec.action.sessionCreate.inheritFrom.session"),
                    &create.inherit_from.session,
                    task_results,
                    definition,
                    Some(&ancestors),
                    false,
                    Some(ActionKind::SessionCreate),
                )?;
                for (key, value) in &create.metadata {
                    validate_value_references(
                        path,
                        &format!("{location}.taskSpec.action.sessionCreate.metadata.{key}"),
                        value,
                        task_results,
                        definition,
                        Some(&ancestors),
                        false,
                        Some(ActionKind::SessionCreate),
                    )?;
                }
                validate_text_references(
                    path,
                    &format!("{location}.taskSpec.action.sessionCreate.firstUserMessage.text"),
                    &create.first_user_message.text,
                    task_results,
                    definition,
                    Some(&ancestors),
                    false,
                    Some(ActionKind::SessionCreate),
                )?;
            }
            if let Some(mask) = &action.session_mask {
                let base = format!("{location}.taskSpec.action.sessionMask");
                for (suffix, value) in [
                    ("sessionId", &mask.session_id),
                    (
                        "masks.subtract[0].selector.overlaps.throughCursor",
                        &mask.masks.subtract[0].selector.overlaps.through_cursor,
                    ),
                    (
                        "masks.add[0].selector.throughCursor",
                        &mask.masks.add[0].selector.through_cursor,
                    ),
                    (
                        "masks.add[0].replacement.text",
                        &mask.masks.add[0].replacement.text,
                    ),
                ] {
                    validate_value_references(
                        path,
                        &format!("{base}.{suffix}"),
                        value,
                        task_results,
                        definition,
                        Some(&ancestors),
                        false,
                        Some(ActionKind::SessionMask),
                    )?;
                }
            }
            if let Some(continuation) = &action.session_continue {
                for (suffix, value) in [
                    ("sessionId", &continuation.session_id),
                    ("fromCursor", &continuation.from_cursor),
                ] {
                    validate_value_references(
                        path,
                        &format!("{location}.taskSpec.action.sessionContinue.{suffix}"),
                        value,
                        task_results,
                        definition,
                        Some(&ancestors),
                        false,
                        Some(ActionKind::SessionContinue),
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    SessionMetadata,
    SessionCreate,
    SessionMask,
    SessionContinue,
}

fn validate_trigger_condition_references(
    path: &str,
    location: &str,
    condition: &RoutineCondition,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    trigger: &RoutineTrigger,
) -> Result<(), AgentDefinitionLoadError> {
    match condition {
        RoutineCondition::Input(condition) => {
            validate_trigger_value_references(
                path,
                &format!("{location}.input"),
                &condition.input,
                task_results,
                definition,
                trigger,
            )?;
            for (index, value) in condition.values.iter().enumerate() {
                validate_trigger_value_references(
                    path,
                    &format!("{location}.values[{index}]"),
                    value,
                    task_results,
                    definition,
                    trigger,
                )?;
            }
        }
        RoutineCondition::Metadata(condition) => {
            for (index, value) in condition.metadata.values.iter().enumerate() {
                validate_trigger_value_references(
                    path,
                    &format!("{location}.metadata.values[{index}]"),
                    value,
                    task_results,
                    definition,
                    trigger,
                )?;
            }
        }
        RoutineCondition::Any(condition) => {
            for (index, child) in condition.any.iter().enumerate() {
                validate_trigger_condition_references(
                    path,
                    &format!("{location}.any[{index}]"),
                    child,
                    task_results,
                    definition,
                    trigger,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_task_condition_references(
    path: &str,
    location: &str,
    condition: &RoutineCondition,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    ancestors: &BTreeSet<&str>,
    action_kind: Option<ActionKind>,
) -> Result<(), AgentDefinitionLoadError> {
    match condition {
        RoutineCondition::Input(condition) => {
            validate_value_references(
                path,
                &format!("{location}.input"),
                &condition.input,
                task_results,
                definition,
                Some(ancestors),
                false,
                action_kind,
            )?;
            for (index, value) in condition.values.iter().enumerate() {
                validate_value_references(
                    path,
                    &format!("{location}.values[{index}]"),
                    value,
                    task_results,
                    definition,
                    Some(ancestors),
                    false,
                    action_kind,
                )?;
            }
        }
        RoutineCondition::Metadata(_) => {}
        RoutineCondition::Any(condition) => {
            for (index, child) in condition.any.iter().enumerate() {
                validate_task_condition_references(
                    path,
                    &format!("{location}.any[{index}]"),
                    child,
                    task_results,
                    definition,
                    ancestors,
                    action_kind,
                )?;
            }
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "trigger conditions require their exact static expression context"
)]
fn validate_trigger_value_references(
    path: &str,
    location: &str,
    value: &RoutineScalar,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    trigger: &RoutineTrigger,
) -> Result<(), AgentDefinitionLoadError> {
    if let RoutineScalar::String(value) = value {
        for expression in expression_references(path, location, value)? {
            validate_static_reference_in_context(
                path,
                location,
                expression,
                task_results,
                definition,
                None,
                false,
                None,
                Some(trigger),
            )?;
        }
    }
    Ok(())
}

fn transitive_ancestors<'a>(task: &'a RoutineTask, tasks: &'a [RoutineTask]) -> BTreeSet<&'a str> {
    let by_name = tasks
        .iter()
        .map(|task| (task.name.as_str(), task))
        .collect::<BTreeMap<_, _>>();
    let mut pending = task
        .run_after
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut ancestors = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if ancestors.insert(name) {
            pending.extend(by_name[name].run_after.iter().map(String::as_str));
        }
    }
    ancestors
}

#[allow(clippy::too_many_arguments)]
fn validate_value_references(
    path: &str,
    location: &str,
    value: &RoutineScalar,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    allowed_tasks: Option<&BTreeSet<&str>>,
    output_allowed: bool,
    action_kind: Option<ActionKind>,
) -> Result<(), AgentDefinitionLoadError> {
    if let RoutineScalar::String(value) = value {
        validate_text_references(
            path,
            location,
            value,
            task_results,
            definition,
            allowed_tasks,
            output_allowed,
            action_kind,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_text_references(
    path: &str,
    location: &str,
    value: &str,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    allowed_tasks: Option<&BTreeSet<&str>>,
    output_allowed: bool,
    action_kind: Option<ActionKind>,
) -> Result<(), AgentDefinitionLoadError> {
    for expression in expression_references(path, location, value)? {
        validate_static_reference(
            path,
            location,
            expression,
            task_results,
            definition,
            allowed_tasks,
            output_allowed,
            action_kind,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_static_reference(
    path: &str,
    location: &str,
    expression: &str,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    allowed_tasks: Option<&BTreeSet<&str>>,
    output_allowed: bool,
    action_kind: Option<ActionKind>,
) -> Result<(), AgentDefinitionLoadError> {
    validate_static_reference_in_context(
        path,
        location,
        expression,
        task_results,
        definition,
        allowed_tasks,
        output_allowed,
        action_kind,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "static reference kinds share one context-sensitive classifier"
)]
fn validate_static_reference_in_context(
    path: &str,
    location: &str,
    expression: &str,
    task_results: &BTreeMap<&str, BTreeMap<&str, RoutineResultType>>,
    definition: &RoutineDefinition,
    allowed_tasks: Option<&BTreeSet<&str>>,
    output_allowed: bool,
    action_kind: Option<ActionKind>,
    active_trigger: Option<&RoutineTrigger>,
) -> Result<(), AgentDefinitionLoadError> {
    if expression.starts_with("event.data.")
        && active_trigger.map_or_else(
            || {
                definition
                    .spec
                    .triggers
                    .iter()
                    .all(|trigger| trigger.event.is_some())
            },
            |trigger| trigger.event.is_some(),
        )
    {
        return Ok(());
    }
    if (expression == "trigger.session" || expression.starts_with("trigger.session."))
        && active_trigger.map_or_else(
            || {
                definition
                    .spec
                    .triggers
                    .iter()
                    .all(|trigger| trigger.manual.is_some() || trigger.session_lifecycle.is_some())
            },
            |trigger| trigger.manual.is_some() || trigger.session_lifecycle.is_some(),
        )
    {
        return Ok(());
    }
    if matches!(
        expression,
        "trigger.context.tokens" | "trigger.context.percent"
    ) && active_trigger.map_or_else(
        || {
            definition
                .spec
                .triggers
                .iter()
                .all(|trigger| trigger.session_lifecycle.is_some())
        },
        |trigger| trigger.session_lifecycle.is_some(),
    ) {
        return Ok(());
    }
    if let Some(name) = expression.strip_prefix("params.") {
        let declared = definition
            .spec
            .params
            .iter()
            .any(|param| param.name == name);
        let exposed_by = |trigger: &RoutineTrigger| {
            trigger
                .manual
                .as_ref()
                .is_some_and(|manual| manual.inputs.iter().any(|input| input.name == name))
                || trigger.interaction.as_ref().is_some_and(|interaction| {
                    interaction.inputs.iter().any(|input| input.name == name)
                })
                || trigger.params.iter().any(|param| param.name == name)
        };
        let exposed = active_trigger.map_or_else(
            || definition.spec.triggers.iter().all(exposed_by),
            exposed_by,
        );
        if declared && exposed {
            return Ok(());
        }
    }
    if let Some(source) = &definition.spec.source_session {
        if expression == "sourceSession.session" || expression.starts_with("sourceSession.session.")
        {
            return Ok(());
        }
        if expression.starts_with("sourceSession.snapshot.") && source.snapshot.is_some() {
            return Ok(());
        }
        if expression == "sourceSession.pause.cursor" && source.pause.is_some() {
            return Ok(());
        }
    }
    if output_allowed
        && (expression == "output.text"
            || expression
                .strip_prefix("output.json.")
                .is_some_and(|field| {
                    !field.is_empty()
                        && field
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                }))
    {
        return Ok(());
    }
    if expression == "action.sessionMetadata.changed"
        && action_kind == Some(ActionKind::SessionMetadata)
    {
        return Ok(());
    }
    if expression == "action.sessionCreate.session.id"
        && action_kind == Some(ActionKind::SessionCreate)
    {
        return Ok(());
    }
    if expression == "action.sessionMask.cursor" && action_kind == Some(ActionKind::SessionMask) {
        return Ok(());
    }
    if let Some(reference) = expression.strip_prefix("tasks.") {
        let parts = reference.split('.').collect::<Vec<_>>();
        if parts.len() == 3
            && parts[1] == "results"
            && task_results
                .get(parts[0])
                .is_some_and(|results| results.contains_key(parts[2]))
            && allowed_tasks.is_none_or(|allowed| allowed.contains(parts[0]))
        {
            return Ok(());
        }
        return Err(invalid(
            path,
            format!("{location} references unknown task result `{expression}`"),
        ));
    }
    Err(invalid(
        path,
        format!("{location} contains unsupported substitution `{expression}`"),
    ))
}

fn validate_dag(
    path: &str,
    tasks: &[RoutineTask],
    indexes: &BTreeMap<&str, usize>,
) -> Result<(), AgentDefinitionLoadError> {
    fn visit(
        index: usize,
        tasks: &[RoutineTask],
        indexes: &BTreeMap<&str, usize>,
        marks: &mut [u8],
    ) -> bool {
        if marks[index] == 1 {
            return false;
        }
        if marks[index] == 2 {
            return true;
        }
        marks[index] = 1;
        for dependency in &tasks[index].run_after {
            if !visit(indexes[dependency.as_str()], tasks, indexes, marks) {
                return false;
            }
        }
        marks[index] = 2;
        true
    }
    let mut marks = vec![0; tasks.len()];
    for index in 0..tasks.len() {
        if !visit(index, tasks, indexes, &mut marks) {
            return Err(invalid(
                path,
                "spec.tasks.runAfter must form an acyclic graph".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_string_list(
    path: &str,
    location: &str,
    values: &[String],
) -> Result<(), AgentDefinitionLoadError> {
    bounded_count(path, location, values.len(), MAX_ROUTINE_LIST_ITEMS, true)?;
    for (index, value) in values.iter().enumerate() {
        validate_string(path, &format!("{location}[{index}]"), value)?;
    }
    Ok(())
}

fn validate_prompt(
    path: &str,
    location: &str,
    value: &str,
) -> Result<(), AgentDefinitionLoadError> {
    validate_string(path, location, value)?;
    expression_references(path, location, value)?;
    Ok(())
}

fn expression_references<'a>(
    path: &str,
    location: &str,
    value: &'a str,
) -> Result<Vec<&'a str>, AgentDefinitionLoadError> {
    if value.len() > MAX_ROUTINE_STRING_BYTES {
        return Err(invalid(
            path,
            format!("{location} exceeds {MAX_ROUTINE_STRING_BYTES} bytes"),
        ));
    }
    let mut references = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("$(") {
        let expression = &rest[start + 2..];
        let Some(end) = expression.find(')') else {
            return Err(invalid(
                path,
                format!("{location} contains an unterminated substitution"),
            ));
        };
        let reference = &expression[..end];
        validate_reference_syntax(path, location, reference)?;
        references.push(reference);
        rest = &expression[end + 1..];
    }
    Ok(references)
}

fn validate_reference_syntax(
    path: &str,
    location: &str,
    inner: &str,
) -> Result<(), AgentDefinitionLoadError> {
    if inner.is_empty()
        || inner.len() > MAX_ROUTINE_EXPRESSION_BYTES
        || inner.contains("$(")
        || inner.contains(')')
        || inner.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
    {
        return Err(invalid(
            path,
            format!("{location} contains an invalid substitution expression"),
        ));
    }
    Ok(())
}

fn validate_expression_scalar(
    path: &str,
    location: &str,
    value: &RoutineScalar,
) -> Result<(), AgentDefinitionLoadError> {
    expression(path, location, value).map(drop)
}

fn expression<'a>(
    path: &str,
    location: &str,
    value: &'a RoutineScalar,
) -> Result<&'a str, AgentDefinitionLoadError> {
    let RoutineScalar::String(value) = value else {
        return Err(invalid(
            path,
            format!("{location} must be a substitution expression"),
        ));
    };
    expression_string(path, location, value)
}

fn expression_string<'a>(
    path: &str,
    location: &str,
    value: &'a str,
) -> Result<&'a str, AgentDefinitionLoadError> {
    if value.len() > MAX_ROUTINE_EXPRESSION_BYTES {
        return Err(invalid(
            path,
            format!("{location} exceeds {MAX_ROUTINE_EXPRESSION_BYTES} expression bytes"),
        ));
    }
    let Some(inner) = value
        .strip_prefix("$(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return Err(invalid(
            path,
            format!("{location} must contain one complete scalar substitution"),
        ));
    };
    validate_reference_syntax(path, location, inner)?;
    Ok(inner)
}

fn validate_parameter_value(
    path: &str,
    location: &str,
    parameter: &RoutineParam,
    value: &RoutineScalar,
    assignment: bool,
) -> Result<(), AgentDefinitionLoadError> {
    if !assignment && matches!(value, RoutineScalar::String(value) if value.contains("$(")) {
        return Err(invalid(
            path,
            format!("{location} must be a literal typed value"),
        ));
    }
    let compatible = match (parameter.r#type, value) {
        (RoutineResultType::String, RoutineScalar::String(_)) => true,
        (RoutineResultType::Boolean, RoutineScalar::Boolean(_)) => true,
        (RoutineResultType::Boolean, RoutineScalar::String(value)) if assignment => {
            matches!(value.as_str(), "true" | "false") || value.starts_with("$(")
        }
        _ => false,
    };
    if !compatible {
        return Err(invalid(
            path,
            format!("{location} is incompatible with parameter type"),
        ));
    }
    validate_scalar(path, location, value, true)?;
    if let (RoutineScalar::String(value), Some(validation)) = (value, &parameter.validation)
        && !value.contains("$(")
    {
        if validation
            .min_length
            .is_some_and(|minimum| value.chars().count() < minimum)
        {
            return Err(invalid(
                path,
                format!("{location} is shorter than validation.minLength"),
            ));
        }
        if let Some(pattern) = &validation.pattern
            && !pattern_matches(pattern, value)
        {
            return Err(invalid(
                path,
                format!("{location} does not match validation.pattern"),
            ));
        }
    }
    Ok(())
}

fn validate_scalar(
    path: &str,
    location: &str,
    value: &RoutineScalar,
    plain_allowed: bool,
) -> Result<(), AgentDefinitionLoadError> {
    if let RoutineScalar::String(value) = value {
        validate_string(path, location, value)?;
        if value.contains("$(") {
            expression_references(path, location, value)?;
        } else if !plain_allowed {
            return Err(invalid(
                path,
                format!("{location} must be a substitution expression"),
            ));
        }
    }
    Ok(())
}

fn validate_string(
    path: &str,
    location: &str,
    value: &str,
) -> Result<(), AgentDefinitionLoadError> {
    if value.trim().is_empty() {
        return Err(invalid(path, format!("{location} must be nonempty")));
    }
    if value.len() > MAX_ROUTINE_STRING_BYTES {
        return Err(invalid(
            path,
            format!("{location} exceeds {MAX_ROUTINE_STRING_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn validate_pattern(
    path: &str,
    location: &str,
    pattern: &str,
) -> Result<(), AgentDefinitionLoadError> {
    if pattern.is_empty() {
        return Err(invalid(path, format!("{location} must be nonempty")));
    }
    if pattern.len() > MAX_ROUTINE_PATTERN_BYTES {
        return Err(invalid(
            path,
            format!("{location} exceeds {MAX_ROUTINE_PATTERN_BYTES} bytes"),
        ));
    }
    compile_pattern(pattern).map_err(|message| invalid(path, format!("{location} {message}")))?;
    Ok(())
}

fn compile_pattern(pattern: &str) -> Result<jaq_interpret::Filter, String> {
    let literal = serde_json::to_string(pattern).expect("a string always serializes as JSON");
    let source = format!("test({literal})");
    let mut definitions = ParseCtx::new(Vec::new());
    definitions.insert_natives(jaq_core::core());
    definitions.insert_defs(jaq_std::std());
    let (parsed, parse_errors) = jaq_parse::parse(&source, jaq_parse::main());
    if !parse_errors.is_empty() {
        return Err("contains an invalid regular expression".to_owned());
    }
    let parsed = parsed.ok_or_else(|| "contains an invalid regular expression".to_owned())?;
    let filter = definitions.compile(parsed);
    if !definitions.errs.is_empty() {
        return Err("contains an invalid regular expression".to_owned());
    }
    let inputs = RcIter::new(std::iter::empty());
    filter
        .run((Ctx::new([], &inputs), Val::from(String::new())))
        .next()
        .ok_or_else(|| "contains an invalid regular expression".to_owned())?
        .map_err(|_| "contains an invalid regular expression".to_owned())?;
    Ok(filter)
}

/// Match an already-admitted bounded parameter pattern against one input value.
pub fn pattern_matches(pattern: &str, value: &str) -> bool {
    let Ok(filter) = compile_pattern(pattern) else {
        return false;
    };
    let inputs = RcIter::new(std::iter::empty());
    filter
        .run((Ctx::new([], &inputs), Val::from(value.to_owned())))
        .any(|result| matches!(result, Ok(Val::Bool(true))))
}

fn bounded_count(
    path: &str,
    location: &str,
    count: usize,
    maximum: usize,
    empty_allowed: bool,
) -> Result<(), AgentDefinitionLoadError> {
    if (!empty_allowed && count == 0) || count > maximum {
        let minimum = usize::from(!empty_allowed);
        return Err(invalid(
            path,
            format!("{location} must contain between {minimum} and {maximum} entries"),
        ));
    }
    Ok(())
}

fn insert_unique<'a>(
    path: &str,
    location: &str,
    name: &'a str,
    names: &mut BTreeMap<&'a str, usize>,
) -> Result<(), AgentDefinitionLoadError> {
    validate_string(path, &format!("{location}.name"), name)?;
    if names.insert(name, names.len()).is_some() {
        return Err(invalid(
            path,
            format!("{location}.name duplicates `{name}`"),
        ));
    }
    Ok(())
}

fn invalid(path: &str, message: String) -> AgentDefinitionLoadError {
    AgentDefinitionLoadError::InvalidResourceValue {
        path: path.to_owned(),
        message,
    }
}
