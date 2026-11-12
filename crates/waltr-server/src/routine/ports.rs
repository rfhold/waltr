//! Consumer-owned replaceable boundaries. No SQL or session implementation types.
use super::{
    definition::Definition,
    history::{Admission, Caller, Invocation, Operation, OperationStatus, Status},
};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};
use waltr_script::{HostContext, HostDispatcher, HostReply, HostRequest};

/// Owned Send future for object-safe consumer ports.
pub type PortFuture<T> = Pin<Box<dyn Future<Output = Result<T, PortError>> + Send + 'static>>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Trusted boundary errors, without adapter payloads or credentials.
pub enum PortError {
    /// Dependency unavailable; commit acknowledgment can be ambiguous.
    Unavailable,
    /// Durable compare-and-swap or immutable identity conflict.
    Conflict,
    /// Global non-overlap authority is already held; no invocation was admitted.
    Busy,
    /// Policy or maintenance denied admission or dispatch.
    Denied,
    /// Required integration has no supplied adapter.
    UnsupportedCapability,
    /// Caller, schema or bound violation.
    Invalid,
    /// Cleanup or durable outcome cannot be verified.
    Uncertain,
}
impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PortError {}

/// Durable observation port, independently replaceable by another store.
pub trait InvocationStore: Send + Sync {
    /// Atomic revision materialization and admission. IDs are not retry tokens.
    fn admit(&self, admission: Admission) -> PortFuture<()>;
    /// Persist actual host-authorized source provenance, never an unvalidated request field.
    fn admit_with_source(
        &self,
        admission: Admission,
        source_session: Option<String>,
    ) -> PortFuture<()> {
        if source_session.is_some() {
            Box::pin(async { Err(PortError::UnsupportedCapability) })
        } else {
            self.admit(admission)
        }
    }
    /// Persist a bounded admission-time public projection without live grants or source text.
    fn admit_with_projection(
        &self,
        admission: Admission,
        source_session: Option<String>,
        public_definition: Option<Value>,
    ) -> PortFuture<()> {
        if public_definition.is_some() {
            Box::pin(async { Err(PortError::UnsupportedCapability) })
        } else {
            self.admit_with_source(admission, source_session)
        }
    }
    /// Compare-and-swap admitted to running exactly once.
    fn running(&self, id: String, now: i64) -> PortFuture<()>;
    /// Commit before the host is invoked; only running invocations accept intents.
    fn dispatch_intent(&self, operation: Operation) -> PortFuture<()>;
    /// Compare-and-swap a dispatch intent to its verified or uncertain outcome.
    fn operation_done(
        &self,
        id: String,
        status: OperationStatus,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<()>;
    /// Atomically preserve uncertainty from any unfinished dispatch.
    fn terminal(
        &self,
        id: String,
        status: Status,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<Status>;
    /// Read one bounded observation; no source or authority materialization.
    fn get(&self, id: String) -> PortFuture<Option<Invocation>>;
    /// Ascending immutable admission cursor, limit 1..=100, no implicit watch/cache.
    fn list(&self, after: i64, limit: u32) -> PortFuture<Vec<Invocation>>;
    /// Ascending immutable operation cursor, limit 1..=100.
    fn operations(&self, invocation: String, after: i64, limit: u32) -> PortFuture<Vec<Operation>>;
    /// Requires exclusive startup ownership of the database, before service construction.
    /// Does not execute guests or redispatch effects; idempotent after the first pass.
    fn reconcile(&self, now: i64) -> PortFuture<u64>;
}
/// Replaceable wall clock for durable observations, not guest deadline enforcement.
pub trait Clock: Send + Sync {
    /// Milliseconds since Unix epoch.
    fn now_ms(&self) -> i64;
}
/// System wall clock; monotonic deadlines remain engine-owned.
#[derive(Debug)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0)
    }
}
/// Drop releases a pre-admission reservation. After admission, finalize must verify
/// pause/owner cleanup before Drop. Failed finalization retains this object in service quarantine.
pub trait Reservation: Send + Sync {
    /// Actual source selected and authorized by this host composition during preflight.
    fn source_session(&self) -> Option<String> {
        None
    }
    /// Admission-time public metadata, including the historical offered start contexts.
    fn public_definition(&self) -> Option<Value> {
        None
    }
    /// Acquire owned coordinator barriers before durable admission. The service joins this future.
    fn prepare(&self) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
    /// Join any resources acquired before durable admission.
    fn abort(&self) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
    /// Validate successful-root obligations after cleanup; errors fail only this invocation.
    fn validate_success(&self) -> Result<(), PortError> {
        Ok(())
    }
    /// Verify owner/pause release after joined host cleanup.
    fn finalize(&self, interrupted: bool) -> PortFuture<()>;
}
/// Supplied adapter owns exact operation schemas, current authorization and safe
/// history projections. Unknown operations must be denied, never forwarded raw.
pub trait OperationPolicy: Send + Sync {
    /// Validate the complete typed request and current authority before intent.
    /// Return only a deliberate safe argument projection.
    fn before(&self, request: &HostRequest, context: &HostContext) -> PortFuture<Value>;
    /// Project the typed response without ambient logs, handles or raw secrets.
    fn project_result(&self, name: &str, reply: &HostReply) -> Result<Option<Value>, PortError>;
}
/// All-capability preflight output with opaque reservation ownership.
pub struct PreparedHost {
    /// Explicit supplied effect adapter, never raw agent or MCP dispatch.
    pub dispatcher: Arc<dyn HostDispatcher>,
    /// Complete operation schema and authorization policy.
    pub policy: Arc<dyn OperationPolicy>,
    /// Preflight reservation; successful admission transfers it to the service.
    pub reservation: Box<dyn Reservation>,
    /// Optional opaque snapshot provenance, not session authority.
    pub snapshot: Option<String>,
}
/// Supplied integration boundary; the service has no implicit capability grants.
pub trait HostFactory: Send + Sync {
    /// Receive the complete already-schema-validated input for admission-pinned
    /// time windows and required nonblank prompts. Defaults preserve older adapters.
    #[allow(
        clippy::too_many_arguments,
        reason = "validated parameters remain separate from trusted invocation identity and caller authority"
    )]
    fn preflight_input(
        &self,
        binding: super::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
        _input: Value,
    ) -> PortFuture<PreparedHost> {
        self.preflight(binding, definition, selected, caller)
    }
    /// Child integration must preserve the supplied snapshot and authority ceiling explicitly.
    #[allow(
        clippy::too_many_arguments,
        reason = "trusted durable identity remains separate from child authority and caller metadata"
    )]
    fn preflight_child(
        &self,
        _binding: super::history::InvocationBinding,
        _definition: Arc<Definition>,
        _selected: SelectedEntrypoint,
        _caller: Caller,
        _scope: super::children::ChildScope,
    ) -> PortFuture<PreparedHost> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Validate all required capabilities and acquire opaque source/owner authority
    /// before durable admission. Missing integrations fail unsupported, not fallback.
    fn preflight(
        &self,
        binding: super::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<PreparedHost>;
}

/// Helper selection is typed: named agents are resolved at admission, restricted profiles
/// are host-built and never inherit a named agent's policy by copying fields.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum HelperSelection {
    /// Exact current agent catalog identity.
    Named {
        /// Exact catalog agent name.
        name: String,
    },
    /// Exact explicitly configured, restricted ad-hoc profile.
    Restricted {
        /// Host-configured restricted profile identity.
        profile: String,
    },
}

/// Fully bounded helper request after operation policy validation.
pub struct HelperRequest {
    /// Pinned selection, not raw provider configuration.
    pub selection: HelperSelection,
    /// Invocation-local task, at most 16384 bytes.
    pub task: String,
    /// Optional declared capture contract identity.
    pub capture: Option<String>,
    /// Total provider turns, including validation corrections.
    pub max_turns: u64,
    /// Total helper deadline; never reset by validation.
    pub deadline: std::time::Duration,
    /// Optional neutral semantic observer.
    pub observer: Option<Arc<dyn waltr_agent::CompletionObserver>>,
}
impl std::fmt::Debug for HelperRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperRequest").finish_non_exhaustive()
    }
}

/// Accepted structured capture, linked to its real post-policy tool result.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperCapture {
    /// Durable original call identity.
    pub call_id: String,
    /// Durable timeline identity.
    pub item_id: waltr_agent::TimelineItemId,
    /// Complete validated post-policy value.
    pub value: Value,
}

/// Only an accepted candidate may cross the helper port.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperResult {
    /// Durable logical session, not permission or resume authority.
    pub session_id: waltr_agent::SessionId,
    /// Pinned named definition digest, or restricted profile identity.
    pub definition_identity: String,
    /// Accepted response; candidate previews are deliberately absent.
    pub response: String,
    /// Declared structured captures, independent of semantic validation.
    pub captures: Vec<HelperCapture>,
    /// Immutable cumulative post-policy evidence.
    pub evidence: Vec<waltr_agent::CompletionToolEvidence>,
}

/// Independently substitutable helper lifecycle; implementations must join on cleanup.
pub trait HelperRunner: Send + Sync {
    /// Bind private invocation-local tool mailboxes without granting tool authority.
    fn bind_tool_streams(&self, _streams: Arc<super::adapters::tool_streams::ToolStreams>) -> PortFuture<()> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Resolve an invocation-local model selector before any helper or attachment effects.
    fn select_inference(&self, selector: &super::adapters::helper::InferenceSelector) -> Result<Option<Arc<dyn HelperRunner>>, PortError> {
        if selector.is_default() { Ok(None) } else { Err(PortError::UnsupportedCapability) }
    }
    /// Reject definitely denied declared captures before upstream effects. Ask remains live.
    /// Adapters without a policy projection must fail closed for declared captures.
    fn preflight_captures(&self) -> PortFuture<()> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Bind the immutable resource's contracts before admission, never granting tools.
    fn bind_captures(&self, captures: std::collections::BTreeMap<String, super::definition::CaptureContract>) -> PortFuture<()> {
        Box::pin(async move { if captures.is_empty() { Ok(()) } else { Err(PortError::UnsupportedCapability) } })
    }
    /// Pin native model capabilities for this exact selection before opening payloads.
    fn input_capabilities(&self, _selection: HelperSelection) -> PortFuture<waltr_agent::TransientModelCapabilities> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// One normal helper with both an authorized source and invocation-local attachments.
    fn run_with_inputs(&self, request: HelperRequest, source: Option<waltr_agent::SessionCopy>, payload: Option<HelperPayload>, cancellation: tokio_util::sync::CancellationToken) -> PortFuture<HelperResult> {
        match payload {
            Some(payload) if source.is_none() => self.run_with_payload(request, payload, cancellation),
            Some(_) => Box::pin(async { Err(PortError::UnsupportedCapability) }),
            None => self.run_with_source(request, source, cancellation),
        }
    }
    /// Validate pinned native-input selection without creating a session or connecting.
    fn preflight_payload(
        &self,
        _selection: HelperSelection,
        _capabilities: waltr_agent::TransientModelCapabilities,
        _capture: Option<String>,
    ) -> PortFuture<()> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Invocation-local payloads and larger host-owned results under a host-pinned profile.
    fn run_with_payload(
        &self,
        _request: HelperRequest,
        _payload: HelperPayload,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> PortFuture<HelperResult> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Use a trusted request-local projection without changing helper policy or ownership.
    fn run_with_source(
        &self,
        request: HelperRequest,
        source: Option<waltr_agent::SessionCopy>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> PortFuture<HelperResult> {
        if source.is_some() {
            Box::pin(async { Err(PortError::UnsupportedCapability) })
        } else {
            self.run(request, cancellation)
        }
    }
    /// Pin every permitted named definition during invocation preflight, before admission.
    /// Implementations without named helpers reject nonempty selections.
    fn pin_named(&self, names: Vec<String>) -> PortFuture<()> {
        Box::pin(async move {
            if names.is_empty() {
                Ok(())
            } else {
                Err(PortError::UnsupportedCapability)
            }
        })
    }
    /// Start a real durable helper; rejection continues that same helper.
    fn run(
        &self,
        request: HelperRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> PortFuture<HelperResult>;
    /// Stop admission, cancel owned work and join actual helpers and validators.
    fn cleanup(&self) -> PortFuture<()>;
}

/// Trusted media-consumer profile; never deserialized from script arguments.
#[derive(Clone, Debug)]
pub struct HelperPayload {
    /// Explicit process-wide attachment credit.
    pub budget: waltr_agent::TransientInputBudget,
    /// Host-authorized native attachments retained through owned helper execution.
    pub attachments: Vec<waltr_agent::TransientAttachment>,
    /// Exact pinned model/API support.
    pub capabilities: waltr_agent::TransientModelCapabilities,
    /// Total encoded completion ceiling, at most 32 MiB.
    pub completion_bytes: usize,
    /// Host-owned response ceiling, at most 1 MiB of UTF-8.
    pub response_bytes: usize,
}

/// Selected named descriptor pinned to the immutable definition; not authority.
#[derive(Clone, Debug)]
pub struct SelectedEntrypoint {
    /// Exact entrypoint key in the definition.
    pub name: String,
    /// Exact descriptor, including the semantic selector for adapter validation.
    pub descriptor: super::definition::Entrypoint,
}
impl std::fmt::Debug for PreparedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedHost").finish_non_exhaustive()
    }
}
