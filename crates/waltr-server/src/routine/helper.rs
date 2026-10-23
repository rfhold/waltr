use std::collections::BTreeMap;

use waltr_agent::{
    AgentDefinitionBundle, InferenceSettings, SessionCreateRequest, SessionId, SessionStart,
};
use serde_json::Value;

pub(super) struct HelperIdentity<'a> {
    pub execution_id: &'a str,
    pub invocation_id: Option<&'a str>,
    pub routine: &'a str,
    pub revision: &'a str,
    pub trigger: &'a str,
    pub task: &'a str,
    pub source: Option<&'a SessionId>,
}

pub(super) fn request(
    identity: HelperIdentity<'_>,
    inference: InferenceSettings,
    bundle: AgentDefinitionBundle,
    start: SessionStart,
) -> SessionCreateRequest {
    let id = SessionId::new(format!(
        "routine_{}",
        super::engine::short_digest(&format!("{}\0{}", identity.execution_id, identity.task))
    ));
    let mut metadata = BTreeMap::from([
        ("waltr.session.ephemeral".to_owned(), Value::Bool(true)),
        (
            "waltr.session.purpose".to_owned(),
            Value::String("routine-task".to_owned()),
        ),
    ]);
    for (key, value) in [
        ("name", identity.routine),
        ("revision", identity.revision),
        ("trigger", identity.trigger),
        ("task", identity.task),
        ("execution_id", identity.execution_id),
    ] {
        metadata.insert(
            format!("waltr.routine.{key}"),
            Value::String(value.to_owned()),
        );
    }
    if let Some(id) = identity.invocation_id {
        metadata.insert(
            "waltr.routine.invocation_id".to_owned(),
            Value::String(id.to_owned()),
        );
    }
    if let Some(source) = identity.source {
        metadata.insert(
            "waltr.routine.source_session_id".to_owned(),
            Value::String(source.as_str().to_owned()),
        );
    }
    SessionCreateRequest {
        id,
        inference_settings: Some(inference),
        agent_definition: Some(bundle),
        metadata_values: metadata,
        start,
    }
}
