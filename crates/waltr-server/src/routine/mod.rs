//! Server-owned event-to-task Routine runtime.

mod engine;
mod event;
mod executor;
mod expression;
mod host;
mod lifecycle;

pub use engine::ManualRoutineHandle;
pub use engine::{EngineHealth, RoutineEngine};
pub use event::{EventBus, EventEnvelope, EventSink, EventSource, USER_MESSAGE_CREATED};

use std::collections::BTreeMap;

/// One typed scalar accepted or returned by a Routine invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutineValue {
    /// UTF-8 string value, preserved exactly.
    String(String),
    /// Boolean value.
    Boolean(bool),
}

impl RoutineValue {
    fn into_json(self) -> serde_json::Value {
        match self {
            Self::String(value) => serde_json::Value::String(value),
            Self::Boolean(value) => serde_json::Value::Bool(value),
        }
    }

    fn from_json(value: serde_json::Value) -> Result<Self, InvocationError> {
        match value {
            serde_json::Value::String(value) => Ok(Self::String(value)),
            serde_json::Value::Bool(value) => Ok(Self::Boolean(value)),
            _ => Err(InvocationError::new(
                "result_type",
                "routine result is not string or boolean",
            )),
        }
    }
}

/// Complete process-local request to invoke one manual Routine trigger.
#[derive(Debug, Clone)]
pub struct ManualRoutineInvocation {
    /// Correlation identity; it does not provide deduplication.
    pub request_id: String,
    /// Materialized Routine name.
    pub routine_name: String,
    /// Manual trigger name.
    pub trigger_name: String,
    /// Session in whose scope the Routine is invoked.
    pub session_id: waltr_agent::SessionId,
    /// Typed values keyed by declared input name.
    pub inputs: BTreeMap<String, RoutineValue>,
}

/// One declaration-ordered top-level Routine result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineInvocationResult {
    /// Declared result name.
    pub name: String,
    /// Runtime-evaluated typed result.
    pub value: RoutineValue,
}

#[derive(Debug)]
/// Process-local Routine admission or execution failure.
pub struct InvocationError {
    /// Stable server-local error class used by the later service adapter.
    pub class: &'static str,
    /// Secret-free failure detail.
    pub message: String,
}

impl InvocationError {
    fn new(class: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

#[cfg(test)]
use std::{
    collections::HashSet,
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(test)]
use waltr_agent::{
    InferenceSettings, MemorySessionStore, SessionCoordinator, SessionCreateRequest, SessionEvent,
    SessionId, SessionStart, SessionStartMessage,
};
#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::resource_catalog::AgentDefinitionState;
#[cfg(test)]
use engine::{
    AdmittedInvocation, DestinationTaskTracker, InvocationCache, RoutineInvocationContext,
    TERMINAL_CACHE_CAPACITY,
};
#[cfg(test)]
use event::session_event_envelope;
#[cfg(test)]
use executor::{agent_bundle, execute_task};
#[cfg(test)]
use expression::{
    MAX_EXPANDED_PROMPT_BYTES, MAX_EXPANDED_STRING_BYTES, MAX_SERIALIZED_METADATA_VALUE_BYTES,
    substitute_scalar, substitute_string,
};
#[cfg(test)]
use host::{CaptureFuture, ChildRunFuture, HostFuture, RoutineHost};

#[cfg(test)]
mod tests;
