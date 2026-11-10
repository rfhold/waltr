//! New invocation observations, never resumable execution state.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Service-issued durable identity, distinct from engine handles and caller correlation.
#[derive(Clone, Debug)]
pub struct InvocationBinding {
    id: String,
    principal: String,
    started_ms: i64,
}
impl InvocationBinding {
    pub(crate) fn new(id: String, principal: String) -> Self {
        Self {
            id,
            principal,
            started_ms: 0,
        }
    }
    pub(crate) fn at_start(mut self, started_ms: i64) -> Self {
        self.started_ms = started_ms;
        self
    }
    /// Single trusted invocation start used by preflight windows and durable admission.
    pub const fn started_ms(&self) -> i64 {
        self.started_ms
    }
    /// Durable observation identity selected before preflight.
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Authenticated principal bound to the observation.
    pub fn principal(&self) -> &str {
        &self.principal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Durable invocation state; terminal states never transition again.
pub enum Status {
    /// Commit precedes guest execution.
    Admitted,
    /// Execution owner acquired the admitted row.
    Running,
    /// Engine join, host cleanup and owner finalization all succeeded.
    Succeeded,
    /// Execution or declared output validation failed.
    Failed,
    /// Cancellation or restart ended execution without a known unresolved effect.
    Interrupted,
    /// An effect or cleanup outcome cannot be verified.
    Uncertain,
}
impl Status {
    /// Whether a durable terminal observation exists.
    pub const fn terminal(self) -> bool {
        !matches!(self, Self::Admitted | Self::Running)
    }
    /// Stable SQL representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Uncertain => "uncertain",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Immutable provenance, independent of the adapter implementation.
pub enum Origin {
    /// Explicit user request.
    Manual,
    /// Event adapter.
    Event,
    /// Schedule adapter.
    Schedule,
    /// Lifecycle adapter.
    Lifecycle,
    /// Interactive client adapter.
    Interactive,
    /// Named Routine tool.
    Tool,
    /// Inline script tool.
    Inline,
    /// MCP adapter.
    Mcp,
    /// Progress adapter.
    Progress,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Explicit logical owner policy; not inferred from a request future's lifetime.
pub enum Ownership {
    /// Service-owned execution survives caller loss.
    Detached,
    /// Handle Drop requests cancellation.
    Leased,
    /// Tool handle Drop requests cancellation.
    Tool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Bounded provenance only; never grants capabilities or pause authority.
pub struct Caller {
    /// Adapter-supplied opaque authenticated principal identity.
    pub principal: String,
    /// Entry adapter family.
    pub origin: Origin,
    /// Explicit execution lifetime policy.
    pub ownership: Ownership,
    /// Correlation only, never a retry or deduplication token.
    pub correlation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Immutable exact program snapshot, stored once for every distinct hash.
pub struct Revision {
    /// SHA-256 identity.
    pub id: String,
    /// Program API identity.
    pub api_version: String,
    /// Language, interpreter and ABI identity.
    pub engine: String,
    /// Canonical envelope with the exact source.
    pub envelope: String,
    /// Catalog name; invocation rows do not duplicate the envelope.
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Atomic revision materialization and invocation admission input.
pub struct Admission {
    /// Unique observation identity, never a replay key.
    pub id: String,
    /// Immutable program snapshot.
    pub revision: Revision,
    /// Immutable caller provenance.
    pub caller: Caller,
    /// Selected declarative entrypoint name.
    pub entrypoint: String,
    /// Host-issued opaque snapshot identity, not raw session state or authority.
    pub snapshot: Option<String>,
    /// Wall-clock admission time, milliseconds since Unix epoch.
    pub admitted_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Bounded durable invocation observation, without resumable guest state.
pub struct Invocation {
    /// Strict durable-record format, currently 1.
    pub version: u32,
    /// Monotonic SQL admission cursor.
    pub cursor: i64,
    /// Unique invocation identity.
    pub id: String,
    /// Foreign key to the exact program snapshot.
    pub revision: String,
    /// Immutable provenance.
    pub caller: Caller,
    /// Selected entrypoint descriptor.
    pub entrypoint: String,
    /// Opaque host-issued source snapshot identity.
    pub snapshot: Option<String>,
    /// Latest durable state.
    pub status: Status,
    /// Admission wall-clock milliseconds.
    pub admitted_ms: i64,
    /// Latest transition wall-clock milliseconds.
    pub updated_ms: i64,
    /// Complete typed output; only durable success exposes it.
    pub result: Option<Value>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Dynamic effect observation; never a declared task graph.
pub enum OperationStatus {
    /// Intent committed before the adapter effect.
    Dispatched,
    /// Adapter response and bounded projection committed.
    Completed,
    /// Adapter verified that the operation failed.
    Failed,
    /// Effect outcome cannot be verified; never replay automatically.
    Uncertain,
}
impl OperationStatus {
    /// Stable SQL representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Bounded safe projection of one dynamic host operation.
pub struct Operation {
    /// Strict durable-record format, currently 1.
    pub version: u32,
    /// Monotonic SQL operation cursor.
    pub cursor: i64,
    /// Unique operation identity.
    pub id: String,
    /// Invocation foreign key.
    pub invocation: String,
    /// Policy-validated operation name.
    pub name: String,
    /// Latest durable effect observation.
    pub status: OperationStatus,
    /// Explicit adapter-owned safe projection, never ambient raw arguments.
    pub arguments: Value,
    /// Optional adapter-owned safe output projection, at most 4096 encoded bytes.
    pub result: Option<Value>,
    /// Latest transition wall-clock milliseconds.
    pub updated_ms: i64,
}
