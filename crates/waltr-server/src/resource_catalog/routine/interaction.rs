//! Accepted interaction catalog contract. Scheduling and transport execution are separate.

use super::*;
use waltr::grpc::waltr::config::v1 as proto;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineInteractionTrigger {
    #[serde(default)]
    pub inputs: Vec<RoutineManualInput>,
    pub input_ports: Vec<RoutineInputPort>,
    pub output_ports: Vec<RoutineOutputPort>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineInputPort {
    pub name: String,
    pub required: bool,
    pub format: RoutineStreamFormat,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineOutputPort {
    pub name: String,
    pub format: RoutineStreamFormat,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum RoutineStreamFormat {
    Audio {
        encoding: RoutineAudioEncoding,
        #[serde(rename = "sampleRateHz")]
        sample_rate_hz: u32,
        channels: u32,
    },
    Text,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum RoutineAudioEncoding {
    #[serde(rename = "pcmS16le")]
    PcmS16Le,
}

/// A stream handle is never a scalar substitution or an interpolated string.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub enum RoutineStreamRef {
    InteractionInput(String),
    TaskOutput(String),
}

impl TryFrom<String> for RoutineStreamRef {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let inner = value
            .strip_prefix("$(")
            .and_then(|value| value.strip_suffix(')'))
            .ok_or("expected one complete stream reference")?;
        let (name, task) = if let Some(name) = inner.strip_prefix("interaction.inputs.") {
            (name, false)
        } else if let Some(name) = inner
            .strip_prefix("tasks.")
            .and_then(|value| value.strip_suffix(".output"))
        {
            (name, true)
        } else {
            return Err(
                "expected interaction.inputs.<port> or tasks.<task>.output stream reference".into(),
            );
        };
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err("invalid stream reference name".into());
        }
        Ok(if task {
            Self::TaskOutput(name.into())
        } else {
            Self::InteractionInput(name.into())
        })
    }
}

impl From<RoutineStreamRef> for String {
    fn from(value: RoutineStreamRef) -> Self {
        match value {
            RoutineStreamRef::InteractionInput(name) => format!("$(interaction.inputs.{name})"),
            RoutineStreamRef::TaskOutput(name) => format!("$(tasks.{name}.output)"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineModelRef {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineTranscribe {
    pub model: RoutineModelRef,
    pub input: RoutineStreamRef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineSynthesize {
    pub model: RoutineModelRef,
    /// Consumers must send committed segments only, never interim hypotheses.
    pub input: RoutineStreamRef,
    pub format: RoutineStreamFormat,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineCollect {
    pub stream: RoutineStreamRef,
    pub max_bytes: usize,
    pub separator: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineStreamOutput {
    pub port: String,
    pub stream_from: RoutineStreamRef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutineWorkGroup {
    pub name: String,
    pub on_busy: RoutineWorkGroupBusy,
    /// Required and positive only for queue policy. No implicit interruption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_queued: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RoutineWorkGroupBusy {
    Queue,
    Reject,
    Interrupt,
}

pub(super) fn validate_model_ref(
    path: &str,
    model: &RoutineModelRef,
) -> Result<(), AgentDefinitionLoadError> {
    for value in [&model.provider, &model.model] {
        validate_string(path, "model reference", value)?;
        if value.contains("$(") {
            return Err(invalid(
                path,
                "model references must be explicit catalog keys".into(),
            ));
        }
    }
    Ok(())
}

fn validate_stream_name(path: &str, name: &str) -> Result<(), AgentDefinitionLoadError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(invalid(
            path,
            "interaction names must be 1..128 ASCII letters, digits, hyphens, or underscores"
                .into(),
        ));
    }
    Ok(())
}

fn validate_format(
    path: &str,
    format: &RoutineStreamFormat,
) -> Result<(), AgentDefinitionLoadError> {
    if let RoutineStreamFormat::Audio {
        sample_rate_hz,
        channels,
        ..
    } = format
    {
        if !(8000..=48000).contains(sample_rate_hz) || !(1..=8).contains(channels) {
            return Err(invalid(
                path,
                "PCM format requires sampleRateHz 8000..48000 and channels 1..8".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_interaction_trigger(
    path: &str,
    interaction: &RoutineInteractionTrigger,
    definition: &RoutineDefinition,
) -> Result<(), AgentDefinitionLoadError> {
    if definition.spec.source_session.is_some() {
        return Err(invalid(
            path,
            "interaction triggers cannot capture a source session".into(),
        ));
    }
    bounded_count(
        path,
        "interaction.inputs",
        interaction.inputs.len(),
        MAX_ROUTINE_PARAMS,
        true,
    )?;
    bounded_count(
        path,
        "interaction.inputPorts",
        interaction.input_ports.len(),
        32,
        false,
    )?;
    bounded_count(
        path,
        "interaction.outputPorts",
        interaction.output_ports.len(),
        32,
        true,
    )?;
    let mut names = BTreeMap::new();
    for input in &interaction.inputs {
        insert_unique(path, "interaction.inputs", &input.name, &mut names)?;
        if !definition
            .spec
            .params
            .iter()
            .any(|param| param.name == input.name)
        {
            return Err(invalid(
                path,
                "interaction input references an undeclared parameter".into(),
            ));
        }
    }
    for param in &definition.spec.params {
        if param.required && !names.contains_key(param.name.as_str()) {
            return Err(invalid(
                path,
                "interaction hides a required parameter".into(),
            ));
        }
    }
    let mut names = BTreeMap::new();
    for port in &interaction.input_ports {
        validate_stream_name(path, &port.name)?;
        insert_unique(path, "interaction.inputPorts", &port.name, &mut names)?;
        validate_format(path, &port.format)?;
    }
    let mut names = BTreeMap::new();
    for port in &interaction.output_ports {
        validate_stream_name(path, &port.name)?;
        insert_unique(path, "interaction.outputPorts", &port.name, &mut names)?;
        validate_format(path, &port.format)?;
    }
    Ok(())
}

pub(super) fn validate_stream_task(
    path: &str,
    task: &RoutineTask,
) -> Result<(), AgentDefinitionLoadError> {
    let spec = &task.task_spec;
    if spec.features.is_some()
        || spec.tools.is_some()
        || spec.permissions.is_some()
        || spec.limits.is_some()
        || spec.output.is_some()
    {
        return Err(invalid(
            path,
            "stream tasks cannot declare agent task fields".into(),
        ));
    }
    if let Some(transcribe) = &spec.transcribe {
        validate_model_ref(path, &transcribe.model)?;
    }
    if let Some(synthesize) = &spec.synthesize {
        validate_model_ref(path, &synthesize.model)?;
        validate_format(path, &synthesize.format)?;
        if !matches!(synthesize.format, RoutineStreamFormat::Audio { .. }) {
            return Err(invalid(path, "synthesis output must be audio".into()));
        }
    }
    if let Some(collect) = &spec.collect {
        if collect.max_bytes == 0 {
            return Err(invalid(
                path,
                "collect.maxBytes must be positive; server limits apply independently".into(),
            ));
        }
        if collect.separator.len() > 1024 {
            return Err(invalid(path, "collect.separator exceeds 1024 bytes".into()));
        }
        if spec
            .results
            .iter()
            .any(|result| result.r#type != RoutineResultType::String)
        {
            return Err(invalid(path, "collector results must be strings".into()));
        }
    } else if !spec.results.is_empty() {
        return Err(invalid(
            path,
            "transcribe and synthesize expose streams, not scalar results".into(),
        ));
    }
    bounded_count(
        path,
        "stream task results",
        spec.results.len(),
        MAX_ROUTINE_RESULTS,
        true,
    )
}

impl RoutineTaskSpec {
    /// Live dependency: start the consumer when this handle is available, not on completion.
    #[must_use]
    pub fn stream_input(&self) -> Option<&RoutineStreamRef> {
        self.transcribe
            .as_ref()
            .map(|task| &task.input)
            .or_else(|| self.synthesize.as_ref().map(|task| &task.input))
            .or_else(|| self.collect.as_ref().map(|task| &task.stream))
    }

    #[must_use]
    pub fn stream_output_format(&self) -> Option<RoutineStreamFormat> {
        if self.transcribe.is_some()
            || self.session_template.is_some()
                && self.output.as_ref().is_some_and(|output| output.stream)
        {
            Some(RoutineStreamFormat::Text)
        } else {
            self.synthesize.as_ref().map(|task| task.format.clone())
        }
    }
}

fn reference_format(
    path: &str,
    definition: &RoutineDefinition,
    reference: &RoutineStreamRef,
) -> Result<RoutineStreamFormat, AgentDefinitionLoadError> {
    match reference {
        RoutineStreamRef::TaskOutput(name) => definition
            .spec
            .tasks
            .iter()
            .find(|task| task.name == *name)
            .and_then(|task| task.task_spec.stream_output_format())
            .ok_or_else(|| invalid(path, format!("unknown stream output from task `{name}`"))),
        RoutineStreamRef::InteractionInput(name) => {
            let mut format = None;
            for trigger in &definition.spec.triggers {
                let port = trigger
                    .interaction
                    .as_ref()
                    .and_then(|interaction| {
                        interaction
                            .input_ports
                            .iter()
                            .find(|port| port.name == *name)
                    })
                    .ok_or_else(|| {
                        invalid(
                            path,
                            format!("stream input `{name}` must exist on every trigger"),
                        )
                    })?;
                if !port.required {
                    return Err(invalid(
                        path,
                        format!("task requires optional input port `{name}`"),
                    ));
                }
                if format.as_ref().is_some_and(|format| *format != port.format) {
                    return Err(invalid(
                        path,
                        format!("input format differs between triggers for `{name}`"),
                    ));
                }
                format = Some(port.format.clone());
            }
            format.ok_or_else(|| invalid(path, "missing interaction trigger".into()))
        }
    }
}

fn validate_live_components(
    path: &str,
    definition: &RoutineDefinition,
    indexes: &BTreeMap<&str, usize>,
    groups: &BTreeMap<&str, usize>,
) -> Result<(), AgentDefinitionLoadError> {
    let tasks = &definition.spec.tasks;
    let mut neighbors = vec![Vec::new(); tasks.len()];
    let mut inputs = BTreeMap::new();
    for (index, task) in tasks.iter().enumerate() {
        let producer = match task.task_spec.stream_input() {
            Some(RoutineStreamRef::TaskOutput(name)) => indexes[name.as_str()],
            Some(RoutineStreamRef::InteractionInput(name)) => {
                *inputs.entry(name).or_insert_with(|| {
                    neighbors.push(Vec::new());
                    neighbors.len() - 1
                })
            }
            None => continue,
        };
        neighbors[index].push(producer);
        neighbors[producer].push(index);
    }
    let mut components = vec![usize::MAX; neighbors.len()];
    let mut count = 0;
    for node in 0..neighbors.len() {
        if components[node] != usize::MAX {
            continue;
        }
        let mut pending = vec![node];
        components[node] = count;
        while let Some(node) = pending.pop() {
            for &neighbor in &neighbors[node] {
                if components[neighbor] == usize::MAX {
                    components[neighbor] = count;
                    pending.push(neighbor);
                }
            }
        }
        count += 1;
    }
    let mut completion_edges = vec![BTreeSet::new(); count];
    let mut indegree = vec![0; count];
    let mut queued_tasks = BTreeMap::new();
    for (index, task) in tasks.iter().enumerate() {
        let component = components[index];
        for dependency in &task.run_after {
            let producer = components[indexes[dependency.as_str()]];
            if producer == component {
                return Err(invalid(
                    path,
                    format!(
                        "task `{}` cannot runAfter `{dependency}` in the same live-stream component",
                        task.name,
                    ),
                ));
            }
            if completion_edges[producer].insert(component) {
                indegree[component] += 1;
            }
        }
        if let Some(group) = task.work_group.as_ref().filter(|name| {
            definition.spec.work_groups[groups[name.as_str()]].on_busy
                == RoutineWorkGroupBusy::Queue
        }) {
            if let Some(previous) = queued_tasks.insert((component, group), task.name.as_str()) {
                return Err(invalid(
                    path,
                    format!(
                        "tasks `{previous}` and `{}` in the same live-stream component cannot share queue work group `{group}`",
                        task.name,
                    ),
                ));
            }
        }
    }
    let mut ready = (0..count)
        .filter(|&node| indegree[node] == 0)
        .collect::<Vec<_>>();
    let mut visited = 0;
    while let Some(node) = ready.pop() {
        visited += 1;
        for &consumer in &completion_edges[node] {
            indegree[consumer] -= 1;
            if indegree[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    if visited != count {
        return Err(invalid(
            path,
            "completion dependencies form a cycle between live-stream components".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_interaction_graph(
    path: &str,
    definition: &RoutineDefinition,
) -> Result<(), AgentDefinitionLoadError> {
    let mut groups = BTreeMap::new();
    bounded_count(
        path,
        "spec.workGroups",
        definition.spec.work_groups.len(),
        64,
        true,
    )?;
    for group in &definition.spec.work_groups {
        validate_stream_name(path, &group.name)?;
        insert_unique(path, "spec.workGroups", &group.name, &mut groups)?;
        if match group.on_busy {
            RoutineWorkGroupBusy::Queue => group
                .max_queued
                .is_none_or(|limit| limit == 0 || limit > 64),
            _ => group.max_queued.is_some(),
        } {
            return Err(invalid(
                path,
                "work group queue policy requires maxQueued 1..64; other policies forbid maxQueued"
                    .into(),
            ));
        }
    }
    let mut dependencies = definition.spec.tasks.clone();
    for (index, task) in definition.spec.tasks.iter().enumerate() {
        if task.task_spec.stream_output_format().is_some()
            || task.task_spec.stream_input().is_some()
        {
            validate_stream_name(path, &task.name)?;
        }
        if task
            .work_group
            .as_ref()
            .is_some_and(|group| !groups.contains_key(group.as_str()))
        {
            return Err(invalid(
                path,
                "task references an unknown work group".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        if task.run_after.iter().any(|name| !seen.insert(name)) {
            return Err(invalid(path, "repeated runAfter dependency".into()));
        }
        if let Some(reference) = task.task_spec.stream_input() {
            let format = reference_format(path, definition, reference)?;
            let audio = matches!(format, RoutineStreamFormat::Audio { .. });
            if audio != task.task_spec.transcribe.is_some() {
                return Err(invalid(
                    path,
                    "stream input format is incompatible with task kind".into(),
                ));
            }
            if let RoutineStreamRef::TaskOutput(producer) = reference {
                dependencies[index].run_after.push(producer.clone());
            }
        }
    }
    let indexes = dependencies
        .iter()
        .enumerate()
        .map(|(index, task)| (task.name.as_str(), index))
        .collect();
    validate_dag(path, &dependencies, &indexes)?;
    validate_live_components(path, definition, &indexes, &groups)?;
    bounded_count(
        path,
        "spec.outputs",
        definition.spec.outputs.len(),
        32,
        true,
    )?;
    let mut outputs = BTreeMap::new();
    for output in &definition.spec.outputs {
        insert_unique(path, "spec.outputs", &output.port, &mut outputs)?;
        let format = reference_format(path, definition, &output.stream_from)?;
        for trigger in &definition.spec.triggers {
            let port = trigger
                .interaction
                .as_ref()
                .and_then(|interaction| {
                    interaction
                        .output_ports
                        .iter()
                        .find(|port| port.name == output.port)
                })
                .ok_or_else(|| {
                    invalid(
                        path,
                        "output routing references undeclared output port".into(),
                    )
                })?;
            if port.format != format {
                return Err(invalid(
                    path,
                    "output stream format does not match port".into(),
                ));
            }
        }
    }
    for trigger in &definition.spec.triggers {
        if let Some(interaction) = &trigger.interaction {
            if interaction
                .output_ports
                .iter()
                .any(|port| !outputs.contains_key(port.name.as_str()))
            {
                return Err(invalid(
                    path,
                    "interaction output port has no stream route".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Validate explicit task selections against a complete pinned catalog snapshot.
/// This is intentionally separate from parsing a single document; no upstream calls occur.
pub(crate) fn validate_routine_models(
    definition: &RoutineDefinition,
    configuration: &proto::Configuration,
    path: &str,
) -> Result<(), AgentDefinitionLoadError> {
    use crate::resource_catalog::speech::{SpeechSettings, resolve_speech_model};
    for task in &definition.spec.tasks {
        if let Some(transcribe) = &task.task_spec.transcribe {
            let resolved = resolve_speech_model(
                configuration,
                &transcribe.model.provider,
                &transcribe.model.model,
                proto::ModelType::Stt,
                true,
            )
            .map_err(|message| invalid(path, message))?;
            let SpeechSettings::Recognition(options) = resolved.settings else {
                unreachable!()
            };
            let format = reference_format(path, definition, &transcribe.input)?;
            if !matches!(format, RoutineStreamFormat::Audio { sample_rate_hz, channels, .. } if sample_rate_hz == options.sample_rate_hz && channels == options.channels)
            {
                return Err(invalid(
                    path,
                    "transcription input PCM format differs from recognition model".into(),
                ));
            }
        }
        if let Some(synthesize) = &task.task_spec.synthesize {
            let resolved = resolve_speech_model(
                configuration,
                &synthesize.model.provider,
                &synthesize.model.model,
                proto::ModelType::Tts,
                true,
            )
            .map_err(|message| invalid(path, message))?;
            let SpeechSettings::Synthesis(options) = resolved.settings else {
                unreachable!()
            };
            if !matches!(synthesize.format, RoutineStreamFormat::Audio { sample_rate_hz, channels, .. } if sample_rate_hz == options.sample_rate_hz && channels == options.channels)
            {
                return Err(invalid(
                    path,
                    "synthesis output PCM format differs from synthesis model".into(),
                ));
            }
        }
        if let Some(model) = task
            .task_spec
            .session_template
            .as_ref()
            .and_then(|session| session.model.as_ref())
        {
            let provider = configuration
                .providers
                .get(&model.provider)
                .ok_or_else(|| invalid(path, "unknown inference provider".into()))?;
            if crate::resource_catalog::speech::is_speech_transport(provider.r#type)
                || !provider
                    .models
                    .get(&model.model)
                    .is_some_and(|model| model.r#type == proto::ModelType::Llm as i32)
            {
                return Err(invalid(
                    path,
                    "standalone inference requires an LLM model on an inference provider".into(),
                ));
            }
        }
    }
    Ok(())
}
