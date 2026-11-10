//! Shell-neutral, versioned Routine v2 projections.
#![allow(missing_docs)]

mod contract;
pub use contract::{RoutineContract, RoutineJson, RoutineSchema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const ROUTINE_INVOCATION_SNAPSHOT_SIZE: u32 = 50;
pub const MAX_RETAINED_ROUTINE_INVOCATIONS: usize = 200;
pub const MAX_RETAINED_ROUTINE_STARTS: usize = 32;
pub const MAX_RETAINED_ROUTINE_DETAILS: usize = 32;
pub const ROUTINE_SOURCE_SESSION_ID_METADATA_KEY: &str = "waltr.routine.source_session_id";

/// Exact integer encoded as a decimal string in every shell snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, TS)]
#[ts(type = "string")]
pub struct RoutineRevision(pub u64);
impl Serialize for RoutineRevision {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for RoutineRevision {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let n = s.parse::<u64>().map_err(serde::de::Error::custom)?;
        if n.to_string() != s {
            return Err(serde::de::Error::custom("noncanonical integer"));
        }
        Ok(Self(n))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientRoutineTriggerKind {
    Manual,
    Event,
    Interactive,
    Tool,
    Inline,
    Mcp,
    Schedule,
    Progress,
    Lifecycle,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub enum RoutineStartContext {
    Global,
    Session,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineEntrypoint {
    pub name: String,
    pub trigger: ClientRoutineTriggerKind,
    pub selector: Option<String>,
    pub start_contexts: Vec<RoutineStartContext>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub enum RoutineDirection {
    Input,
    Output,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub enum RoutineMedia {
    Audio,
    Text,
    Image,
    File,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutinePort {
    pub name: String,
    pub direction: RoutineDirection,
    pub media: RoutineMedia,
    pub required: bool,
    pub max_bytes: RoutineRevision,
    pub formats: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineBudgets {
    pub value_bytes: u32,
    pub result_bytes: u32,
    pub host_calls: u32,
    pub in_flight: u32,
    pub deadline_ms: RoutineRevision,
}
macro_rules! routine_limits {
    (small: [$($small:ident),*], wide: [$($wide:ident),*]) => {
        #[derive(Clone,Debug,Deserialize,Serialize,Eq,PartialEq,TS)]
        #[serde(rename_all="camelCase")]
        pub struct RoutineLimits { $(pub $small:u32,)* $(pub $wide:RoutineRevision,)* }
        impl RoutineLimits {
            /// Whether a negotiated set only narrows the catalog ceilings.
            pub fn is_within(&self,other:&Self) -> bool { true $(&& self.$small<=other.$small)* $(&& self.$wide<=other.$wide)* }
        }
        impl From<waltr::grpc::waltr::routine::v2::ServerLimits> for RoutineLimits {
            fn from(l:waltr::grpc::waltr::routine::v2::ServerLimits) -> Self { Self { $($small:l.$small,)* $($wide:RoutineRevision(l.$wide),)* } }
        }
        impl From<RoutineLimits> for waltr::grpc::waltr::routine::v2::ServerLimits {
            fn from(l:RoutineLimits) -> Self { Self { $($small:l.$small,)* $($wide:l.$wide.0,)* } }
        }
    };
}
routine_limits! {
    small: [chunk_bytes,input_batch_bytes,input_batch_events,edge_bytes,edge_chunks,streams_per_direction,text_segments,retained_text_bytes,image_dimension,helper_attachments,text_results,page_size,structured_value_bytes,definition_bytes,definitions_per_snapshot,definition_snapshot_bytes,operation_projection_bytes,blob_objects,blob_readers],
    wide: [input_bytes,output_bytes,input_duration_ms,total_duration_ms,queue_wait_ms,idle_ms,attachment_lease_ms,attachment_attach_timeout_ms,attachment_absolute_lifetime_ms,file_bytes,image_bytes,image_pixels,helper_attachment_bytes,helper_source_bytes,helper_encoded_bytes,binary_output_lease_ms,text_result_bytes,text_results_encoded_bytes,blob_invocation_bytes,blob_principal_bytes,ordinary_deadline_ms]
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientManualRoutineDefinition {
    pub routine_name: String,
    pub description: String,
    pub routine_revision: String,
    pub api_version: String,
    pub engine: String,
    pub entrypoints: Vec<RoutineEntrypoint>,
    pub parameters: RoutineContract,
    pub result: RoutineContract,
    pub ports: Vec<RoutinePort>,
    pub capabilities: Vec<String>,
    pub budgets: RoutineBudgets,
}
impl ClientManualRoutineDefinition {
    pub fn permits(&self, entrypoint: &str, interactive: bool, source: Option<&str>) -> bool {
        if source.is_some_and(str::is_empty) {
            return false;
        }
        self.entrypoints.iter().any(|e| {
            e.name == entrypoint
                && e.trigger
                    == if interactive {
                        ClientRoutineTriggerKind::Interactive
                    } else {
                        ClientRoutineTriggerKind::Manual
                    }
                && e.start_contexts.contains(&if source.is_some() {
                    RoutineStartContext::Session
                } else {
                    RoutineStartContext::Global
                })
        })
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientRoutineInvocationStatus {
    Admitted,
    Running,
    Succeeded,
    Failed,
    Interrupted,
    Uncertain,
}
impl ClientRoutineInvocationStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Admitted | Self::Running)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub enum RoutineOwnership {
    Detached,
    Leased,
    Tool,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineTextResult {
    pub id: String,
    pub name: String,
    pub sha256: String,
    pub utf8_bytes: RoutineRevision,
    pub encoded_charge: RoutineRevision,
    pub helper_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineHelperReference {
    pub session_id: String,
    pub definition_identity: String,
    pub operation_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineImageInfo {
    pub format: String,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineBlobDescriptor {
    pub blob_id: String,
    pub display_name: String,
    pub content_type: String,
    pub bytes: RoutineRevision,
    pub sha256: Vec<u8>,
    pub image: Option<RoutineImageInfo>,
    pub remaining_validity_ms: Option<RoutineRevision>,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineInvocationSummary {
    pub version: u32,
    pub admission_sequence: RoutineRevision,
    pub invocation_id: String,
    pub routine_name: String,
    pub routine_revision: String,
    pub trigger_name: String,
    pub trigger_kind: ClientRoutineTriggerKind,
    pub ownership: RoutineOwnership,
    pub correlation_id: String,
    pub snapshot: Option<String>,
    pub source_session: Option<String>,
    pub status: ClientRoutineInvocationStatus,
    pub admitted_at_ms: i64,
    pub updated_at_ms: i64,
    pub result: Option<RoutineJson>,
    pub text_results: Vec<RoutineTextResult>,
    pub binary_results: Vec<RoutineBlobDescriptor>,
    pub helpers: Vec<RoutineHelperReference>,
    pub observation_revision: RoutineRevision,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub enum RoutineOperationStatus {
    Dispatched,
    Completed,
    Failed,
    Uncertain,
}

/// Shared server-data requests; mutations are never automatically retried.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[allow(missing_docs)]
pub enum RoutineRequest {
    Invocations {
        #[serde(rename = "afterCursor")]
        after_cursor: RoutineRevision,
        limit: u32,
        #[serde(rename = "sourceSession")]
        source_session: Option<String>,
    },
    Operations {
        #[serde(rename = "invocationId")]
        invocation_id: String,
        #[serde(rename = "afterCursor")]
        after_cursor: RoutineRevision,
        limit: u32,
    },
    Cancel {
        #[serde(rename = "invocationId")]
        invocation_id: String,
    },
}
/// Bounded keyed receipt with an authoritative selected invocation page.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[allow(missing_docs)]
pub struct RoutineRequestState {
    pub request_id: String,
    pub command: RoutineRequest,
    pub status: RoutineRequestStatus,
    pub error: Option<String>,
    pub next_cursor: Option<RoutineRevision>,
    pub cancellation_status: Option<ClientRoutineInvocationStatus>,
    pub selected_invocations: Vec<ClientRoutineInvocationSummary>,
}
/// Local keyed request phase.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[allow(missing_docs)]
pub enum RoutineRequestStatus {
    Loading,
    Ready,
    Failed,
    Expired,
}
/// Transport result consumed by the reducer, not retained as a second collection.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum RoutineRequestResult {
    Invocations(ClientRoutineInvocationPage),
    Operations {
        invocation_id: String,
        operations: Vec<RoutineOperation>,
        next_cursor: Option<RoutineRevision>,
    },
    Cancelled(ClientRoutineInvocationStatus),
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineOperation {
    pub version: u32,
    pub cursor: RoutineRevision,
    pub id: String,
    pub invocation_id: String,
    pub name: String,
    pub status: RoutineOperationStatus,
    pub arguments: Option<RoutineJson>,
    pub result: Option<RoutineJson>,
    pub updated_ms: i64,
    pub observation_revision: RoutineRevision,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineInvocationDetail {
    pub summary: ClientRoutineInvocationSummary,
    pub pinned_definition: ClientManualRoutineDefinition,
    pub operations: Vec<RoutineOperation>,
    pub next_operation_cursor: Option<RoutineRevision>,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineFieldViolation {
    pub field_path: String,
    pub message: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientRoutineValidationFailure {
    pub message: String,
    pub violations: Vec<ClientRoutineFieldViolation>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartManualRoutineArgs {
    pub request_id: String,
    pub routine_name: String,
    pub routine_revision: String,
    pub trigger_name: String,
    pub source_session_id: Option<String>,
    pub parameters: RoutineJson,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientRoutineStartOutcome {
    Accepted(String),
    ValidationFailed(ClientRoutineValidationFailure),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineDefinitionSnapshot {
    pub digest: String,
    pub definitions: Vec<ClientManualRoutineDefinition>,
    pub supported_capabilities: Vec<String>,
    pub limits: waltr::grpc::waltr::routine::v2::ServerLimits,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutineObservationPhase {
    Snapshot,
    Changes,
    ResetRequired,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineInvocationWatchUpdate {
    pub invocations: Vec<ClientRoutineInvocationSummary>,
    pub operations: Vec<RoutineOperation>,
    pub cursor: String,
    pub snapshot_complete: bool,
    pub phase: RoutineObservationPhase,
    pub snapshot_high_water: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRoutineInvocationPage {
    pub invocations: Vec<ClientRoutineInvocationSummary>,
    pub older_page_token: Option<u64>,
}
