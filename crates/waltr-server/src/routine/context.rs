//! Invocation-bound session primitives. Source identities are freshness, never authority.

use super::ports::{PortError, PortFuture};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub const CONTEXT_VALUE_BYTES: usize = 65_536;
pub const CAPABILITIES: &[&str] = &[
    "context.snapshot", "context.original", "context.history", "context.metadata",
    "context.transact", "context.create", "context.start", "context.continue",
];

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", content = "arguments", deny_unknown_fields)]
pub enum ContextOperation {
    #[serde(rename = "context.snapshot")]
    Snapshot(Empty),
    #[serde(rename = "context.original")]
    Original(Empty),
    #[serde(rename = "context.history")]
    History(History),
    #[serde(rename = "context.metadata")]
    Metadata(MetadataRead),
    #[serde(rename = "context.transact")]
    Transact(Transaction),
    #[serde(rename = "context.create")]
    Create(Create),
    #[serde(rename = "context.start")]
    Start(Start),
    #[serde(rename = "context.continue")]
    Continue(Continue),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct History {
    pub source: String,
    pub item_ids: Option<Vec<waltr_agent::TimelineItemId>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MetadataRead {
    pub source: String,
    pub item_id: Option<waltr_agent::TimelineItemId>,
    pub key: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum MetadataValue {
    Absent,
    Present { value: Value },
}
impl MetadataValue {
    pub fn into_option(self) -> Option<Value> {
        match self { Self::Absent => None, Self::Present { value } => Some(value) }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MetadataEntry {
    pub item_id: Option<waltr_agent::TimelineItemId>,
    pub key: String,
    pub value: MetadataValue,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MaskAddition {
    pub key: String,
    pub through: String,
    pub item_ids: Option<Vec<waltr_agent::TimelineItemId>>,
    pub replacement: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Masks {
    pub expected: Vec<(String, waltr_agent::SessionMaskId)>,
    pub remove_keys: Vec<String>,
    pub remove_overlaps: Vec<String>,
    pub add: Vec<MaskAddition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Transaction {
    pub source: String,
    pub expected_revision: String,
    pub predicates: Vec<MetadataEntry>,
    pub updates: Vec<MetadataEntry>,
    pub masks: Option<Masks>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Create {
    pub source: String,
    pub copy_history: bool,
    pub inherit_definition: bool,
    pub inherit_inference: bool,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start { pub destination: String }

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Continue { pub cursor: String }

impl ContextOperation {
    pub fn parse(name: &str, arguments: Value) -> Result<Self, PortError> {
        if serde_json::to_vec(&arguments).map_err(|_| PortError::Invalid)?.len() > CONTEXT_VALUE_BYTES {
            return Err(PortError::Invalid);
        }
        serde_json::from_value(serde_json::json!({"operation":name,"arguments":arguments}))
            .map_err(|_| PortError::Invalid)
    }
}

pub trait ContextPort: Send + Sync {
    fn intent(&self, _operation: &ContextOperation) -> Result<Value, PortError> { Ok(serde_json::json!({})) }
    fn resolve_source(&self, _source: &str) -> Result<waltr_agent::SessionCopy, PortError> {
        Err(PortError::Denied)
    }
    fn prepare(&self) -> PortFuture<()> { Box::pin(async { Ok(()) }) }
    fn snapshot_identity(&self) -> Option<waltr_agent::ContextIdentity> { None }
    fn check(&self, operation: &ContextOperation) -> Result<(), PortError>;
    fn execute(&self, operation: ContextOperation, cancellation: CancellationToken) -> PortFuture<Value>;
    fn cleanup(&self) -> PortFuture<()>;
}

pub const MANAGE_CONTEXT_TOOL_NAME: &str = "manage-context";
pub fn manage_context_descriptor() -> waltr_agent::ToolDefinition {
    waltr_agent::ToolDefinition::new(MANAGE_CONTEXT_TOOL_NAME, "Manage the authorized session context.",
        serde_json::json!({"type":"object","properties":{"instruction":{"type":"string","maxLength":8192}},"required":["instruction"],"additionalProperties":false}))
}

pub(crate) fn bounded(value: Value) -> Result<Value, PortError> {
    if serde_json::to_vec(&value).map_err(|_| PortError::Invalid)?.len() > CONTEXT_VALUE_BYTES {
        return Err(PortError::Invalid);
    }
    Ok(value)
}
