//! Shell-neutral Routine contract values.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Number of newest invocations requested by the shared history watch.
pub const ROUTINE_INVOCATION_SNAPSHOT_SIZE: u32 = 50;
/// Canonical metadata key linking a Routine invocation to its source session.
pub const ROUTINE_SOURCE_SESSION_ID_METADATA_KEY: &str = "waltr.routine.source_session_id";
/// Maximum summaries retained after adding older pages.
pub const MAX_RETAINED_ROUTINE_INVOCATIONS: usize = 200;
/// Maximum completed or pending local start operations retained for command correlation.
pub const MAX_RETAINED_ROUTINE_STARTS: usize = 32;
/// Maximum keyed detail loads retained by the client.
pub const MAX_RETAINED_ROUTINE_DETAILS: usize = 32;

/// Closed value accepted and returned by Routines.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum ClientRoutineValue {
    /// Exact string value.
    String(String),
    /// Boolean value.
    Boolean(bool),
}

/// Closed Routine value type declaration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientRoutineValueType {
    /// String value.
    String,
    /// Boolean value.
    Boolean,
}

/// Closed manual Routine scope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientRoutineScope {
    /// One source session supplied only when starting the Routine.
    Session,
}

/// Trigger category pinned at invocation admission.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientRoutineTriggerKind {
    /// Manual trigger.
    Manual,
    /// Event trigger.
    Event,
}

/// Durable invocation status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientRoutineInvocationStatus {
    /// Accepted and durably admitted.
    Admitted,
    /// Execution has started.
    Running,
    /// Execution succeeded.
    Succeeded,
    /// Execution failed.
    Failed,
    /// Execution was interrupted.
    Interrupted,
}

impl ClientRoutineInvocationStatus {
    /// Return whether this status is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }
}

/// Durable task status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientRoutineTaskStatus {
    /// Declared but not started.
    Pending,
    /// Task execution has started.
    Running,
    /// Task succeeded.
    Succeeded,
    /// Task was skipped.
    Skipped,
    /// Task was blocked by an earlier outcome.
    Blocked,
    /// Task failed.
    Failed,
    /// Task was interrupted.
    Interrupted,
}

impl ClientRoutineTaskStatus {
    /// Return whether this status is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Skipped | Self::Blocked | Self::Failed | Self::Interrupted
        )
    }
}

/// One ordered manual Routine input declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineInputDefinition {
    /// Input name.
    pub name: String,
    /// Closed input type.
    pub r#type: ClientRoutineValueType,
    /// Whether the input is required.
    pub required: bool,
    /// Optional minimum string length.
    pub minimum_string_length: Option<u64>,
    /// Optional string pattern.
    pub string_pattern: Option<String>,
}

/// One ordered manual Routine result declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineResultDefinition {
    /// Result name.
    pub name: String,
    /// Closed result type.
    pub r#type: ClientRoutineValueType,
}

/// One global manual Routine definition in canonical declaration order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientManualRoutineDefinition {
    /// Routine name.
    pub routine_name: String,
    /// Manual trigger name.
    pub trigger_name: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Human-readable description.
    pub description: String,
    /// Closed scope.
    pub scope: ClientRoutineScope,
    /// Inputs in declaration order.
    pub inputs: Vec<ClientRoutineInputDefinition>,
    /// Results in declaration order.
    pub results: Vec<ClientRoutineResultDefinition>,
    /// Immutable definition revision used by new admissions.
    pub routine_revision: String,
}

/// One named typed result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineResult {
    /// Result name.
    pub name: String,
    /// Typed result value.
    pub value: ClientRoutineValue,
}

/// Sanitized Routine failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineFailure {
    /// Stable failure code.
    pub code: String,
    /// Safe failure text.
    pub message: String,
}

/// One user-correctable field violation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineFieldViolation {
    /// Input field path.
    pub field_path: String,
    /// Validation message.
    pub message: String,
}

/// Structured user-correctable start validation failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineValidationFailure {
    /// General validation message.
    pub message: String,
    /// Violations in server order.
    pub violations: Vec<ClientRoutineFieldViolation>,
}

/// Durable invocation summary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineInvocationSummary {
    /// Monotonic admission order.
    pub admission_sequence: u64,
    /// Server-generated durable public identity.
    pub invocation_id: String,
    /// Routine name pinned at admission.
    pub routine_name: String,
    /// Routine revision pinned at admission.
    pub routine_revision: String,
    /// Trigger name pinned at admission.
    pub trigger_name: String,
    /// Trigger category.
    pub trigger_kind: ClientRoutineTriggerKind,
    /// Correlation identifier supplied by the trigger.
    pub correlation_id: String,
    /// Immutable trigger metadata.
    pub metadata: BTreeMap<String, String>,
    /// Event source cursor when applicable.
    pub source_cursor: Option<String>,
    /// Current durable status.
    pub status: ClientRoutineInvocationStatus,
    /// Admission timestamp in Unix milliseconds.
    pub admitted_at_ms: i64,
    /// Start timestamp in Unix milliseconds.
    pub started_at_ms: Option<i64>,
    /// Completion timestamp in Unix milliseconds.
    pub completed_at_ms: Option<i64>,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Sanitized terminal failure.
    pub terminal_error: Option<ClientRoutineFailure>,
    /// Latest durable event sequence represented by this summary.
    pub latest_sequence: u64,
}

/// One task pinned to an invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineTask {
    /// Declared task name.
    pub name: String,
    /// Declared task type.
    pub r#type: String,
    /// Current task status.
    pub status: ClientRoutineTaskStatus,
    /// Ordered task results.
    pub results: Vec<ClientRoutineResult>,
    /// Helper or destination session references by declared key.
    pub session_references: BTreeMap<String, String>,
    /// Sanitized task failure.
    pub error: Option<ClientRoutineFailure>,
}

/// Full durable invocation detail independent of live definitions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineInvocationDetail {
    /// Durable invocation summary.
    pub summary: ClientRoutineInvocationSummary,
    /// Tasks in declaration order.
    pub tasks: Vec<ClientRoutineTask>,
    /// Ordered terminal invocation results.
    pub results: Vec<ClientRoutineResult>,
}

/// Arguments sent through the start transport boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartManualRoutineArgs {
    /// Correlation-only request identity.
    pub request_id: String,
    /// Routine name.
    pub routine_name: String,
    /// Manual trigger name.
    pub trigger_name: String,
    /// Source session supplied at start time only.
    pub source_session_id: String,
    /// Typed inputs keyed by declared name.
    pub inputs: BTreeMap<String, ClientRoutineValue>,
}

/// Validated start transport outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientRoutineStartOutcome {
    /// Work was durably admitted under this server identity.
    Accepted(String),
    /// Work was rejected before durable admission.
    ValidationFailed(ClientRoutineValidationFailure),
}

/// Complete global definition snapshot from the definition watch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineDefinitionSnapshot {
    /// Stable digest of this complete projection.
    pub digest: String,
    /// Definitions in canonical order.
    pub definitions: Vec<ClientManualRoutineDefinition>,
}

/// Bounded history snapshot from the invocation watch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineInvocationSnapshot {
    /// Newest-first invocation summaries.
    pub invocations: Vec<ClientRoutineInvocationSummary>,
    /// High-water event sequence represented by this snapshot.
    pub through_sequence: u64,
    /// Exclusive admission cursor for an older page.
    pub older_page_token: Option<u64>,
}

/// One immutable task declaration carried by admission replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineTaskDescriptor {
    /// Task name.
    pub name: String,
    /// Task type.
    pub r#type: String,
}

/// Immutable admission metadata carried by replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineAdmission {
    /// Exact aggregate pagination and ordering key.
    pub admission_sequence: u64,
    /// Routine name.
    pub routine_name: String,
    /// Pinned Routine revision.
    pub routine_revision: String,
    /// Trigger name.
    pub trigger_name: String,
    /// Trigger category.
    pub trigger_kind: ClientRoutineTriggerKind,
    /// Trigger correlation identifier.
    pub correlation_id: String,
    /// Immutable trigger metadata.
    pub metadata: BTreeMap<String, String>,
    /// Event source cursor.
    pub source_cursor: Option<String>,
    /// Admission timestamp.
    pub admitted_at_ms: i64,
    /// Tasks in declaration order.
    pub declared_tasks: Vec<ClientRoutineTaskDescriptor>,
}

/// Closed event kind used by invocation replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRoutineEventKind {
    /// Invocation admission.
    InvocationAdmitted,
    /// Invocation execution started.
    InvocationStarted,
    /// Task started.
    TaskStarted,
    /// Task succeeded.
    TaskSucceeded,
    /// Task skipped.
    TaskSkipped,
    /// Task blocked.
    TaskBlocked,
    /// Task failed.
    TaskFailed,
    /// Task interrupted.
    TaskInterrupted,
    /// Invocation succeeded.
    InvocationSucceeded,
    /// Invocation failed.
    InvocationFailed,
    /// Invocation interrupted.
    InvocationInterrupted,
}

/// One validated durable replay event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineInvocationEvent {
    /// Durable event cursor.
    pub sequence: u64,
    /// Durable invocation identity.
    pub invocation_id: String,
    /// Closed event kind.
    pub kind: ClientRoutineEventKind,
    /// Task identity for task events only.
    pub task_name: Option<String>,
    /// Event timestamp in Unix milliseconds.
    pub occurred_at_ms: i64,
    /// Sanitized failure when applicable.
    pub error: Option<ClientRoutineFailure>,
    /// Ordered result values when applicable.
    pub values: Vec<ClientRoutineResult>,
    /// Session references when applicable.
    pub session_references: BTreeMap<String, String>,
    /// Admission metadata for admission events only.
    pub admission: Option<ClientRoutineAdmission>,
}

/// Update delivered by the invocation watch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientRoutineInvocationWatchUpdate {
    /// Authoritative bounded snapshot.
    Snapshot(ClientRoutineInvocationSnapshot),
    /// One replay event after the requested cursor.
    Event(ClientRoutineInvocationEvent),
}

/// One validated older-history page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineInvocationPage {
    /// Newest-first summaries before the requested cursor.
    pub invocations: Vec<ClientRoutineInvocationSummary>,
    /// Next exclusive admission cursor.
    pub older_page_token: Option<u64>,
}
