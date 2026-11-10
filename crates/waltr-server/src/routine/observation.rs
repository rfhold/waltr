//! Principal-scoped durable observations; never execution or transport authority.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{
    history::{Invocation, Operation, Revision},
    ports::{PortError, PortFuture},
    text_results::TextResultDescriptor,
};

/// Exact authenticated observation partition, including an optional source-session filter.
#[derive(Clone, Debug)]
pub struct ObservationScope {
    /// Domain-separated authenticated Routine principal.
    pub principal: String,
    /// Exact trusted source provenance, or all authorized sources when absent.
    pub source_session: Option<String>,
}

/// An invocation and selected immutable descriptor state from one committed view.
#[derive(Clone, Debug)]
pub struct InvocationView {
    /// Original durable record with its current status/result.
    pub invocation: Invocation,
    /// Strict mutation revision, independent of admission order and wall time.
    pub observation_revision: i64,
    /// Actual host-authorized source, never caller correlation.
    pub source_session: Option<String>,
    /// Selected business-result descriptors, without content.
    pub text_results: Vec<TextResultDescriptor>,
    /// Immutable exact program revision, used only for the public projection.
    pub revision: Revision,
    /// Exact admission-time public projection; absent only for older/internal admissions.
    pub public_definition: Option<serde_json::Value>,
    /// Metadata selected by the host; no live grants or time-varying recomputation.
    pub publications: Vec<Publication>,
}

/// One bounded operation observation at a committed mutation revision.
#[derive(Clone, Debug)]
pub struct OperationView {
    /// Original safe operation projection.
    pub operation: Operation,
    /// Strict mutation revision.
    pub observation_revision: i64,
}

/// A row in the durable combined observation namespace.
#[derive(Clone, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "bounded pages carry owned invocation projections without an extra allocation per row"
)]
pub enum Observation {
    /// Invocation upsert, including selected descriptors.
    Invocation(InvocationView),
    /// Operation upsert.
    Operation(OperationView),
}
/// Explicit host-selected metadata publication, never raw helper input or output authority.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Publication {
    /// Ordinary helper provenance only.
    Helper {
        /// Actual owned session identity.
        session_id: String,
        /// Exact ordinary definition identity.
        definition_identity: String,
    },
    /// Binary metadata frozen when the fixed lease is transferred.
    Blob {
        /// Public scoped identity, not private storage authority.
        blob_id: String,
        /// Exact declared output port.
        port: String,
        /// Display-only name.
        display_name: String,
        /// Declared MIME; not validation proof.
        content_type: String,
        /// Immutable payload size.
        bytes: u64,
        /// Exact digest bytes.
        sha256: [u8; 32],
        /// Validated format enum (1 PNG, 2 JPEG, 3 WebP), width and height.
        image: Option<(u32, u32, u32)>,
        /// Observation-time validity, frozen under this revision, never a grant.
        remaining_validity_ms: u64,
    },
}

/// A consistent bounded page with its fixed snapshot/change position.
#[derive(Clone, Debug)]
pub struct ObservationPage {
    /// At most the requested combined record limit.
    pub rows: Vec<Observation>,
    /// Last fully delivered journal position.
    pub position: i64,
    /// Fixed committed high-water for this page sequence.
    pub high_water: i64,
    /// True when all snapshot rows through the high-water have been delivered.
    pub complete: bool,
}

/// SQL implementations authorize before limiting or materializing payloads.
pub trait ObservationStore: Send + Sync {
    /// Durable private cursor signing material; never returned to callers or logged.
    fn cursor_key(&self) -> PortFuture<[u8; 32]>;
    /// Atomically publish bounded host metadata and its matching observation revision.
    fn publish(
        &self,
        scope: ObservationScope,
        id: String,
        publication: Publication,
    ) -> PortFuture<()>;
    /// Exact authorized detail and immutable historical program, from one view.
    fn get(&self, scope: ObservationScope, id: String) -> PortFuture<Option<InvocationView>>;
    /// Immutable admission-order pagination, authorized before LIMIT.
    fn list(
        &self,
        scope: ObservationScope,
        after: i64,
        limit: u32,
    ) -> PortFuture<Vec<InvocationView>>;
    /// Authorized operation pagination; foreign and absent parents are indistinguishable.
    fn operations(
        &self,
        scope: ObservationScope,
        parent: String,
        after: i64,
        limit: u32,
    ) -> PortFuture<Vec<OperationView>>;
    /// Initial high-water or continuation; snapshot pages reconstruct latest rows as of it.
    #[allow(
        clippy::too_many_arguments,
        reason = "scope and fixed snapshot continuation remain explicit independent read inputs"
    )]
    fn page(
        &self,
        scope: ObservationScope,
        high_water: Option<i64>,
        after: i64,
        snapshot: bool,
        limit: u32,
    ) -> PortFuture<ObservationPage>;
}

/// Validate bounded observations before querying a dependency.
pub(crate) fn validate(scope: &ObservationScope, after: i64, limit: u32) -> Result<(), PortError> {
    if scope.principal.is_empty()
        || scope.principal.len() > 256
        || after < 0
        || !(1..=100).contains(&limit)
        || scope
            .source_session
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
    {
        return Err(PortError::Invalid);
    }
    Ok(())
}

impl<T: ObservationStore + ?Sized> ObservationStore for Arc<T> {
    fn cursor_key(&self) -> PortFuture<[u8; 32]> {
        (**self).cursor_key()
    }
    fn publish(
        &self,
        scope: ObservationScope,
        id: String,
        publication: Publication,
    ) -> PortFuture<()> {
        (**self).publish(scope, id, publication)
    }
    fn get(&self, scope: ObservationScope, id: String) -> PortFuture<Option<InvocationView>> {
        (**self).get(scope, id)
    }
    fn list(
        &self,
        scope: ObservationScope,
        after: i64,
        limit: u32,
    ) -> PortFuture<Vec<InvocationView>> {
        (**self).list(scope, after, limit)
    }
    fn operations(
        &self,
        scope: ObservationScope,
        parent: String,
        after: i64,
        limit: u32,
    ) -> PortFuture<Vec<OperationView>> {
        (**self).operations(scope, parent, after, limit)
    }
    fn page(
        &self,
        scope: ObservationScope,
        high_water: Option<i64>,
        after: i64,
        snapshot: bool,
        limit: u32,
    ) -> PortFuture<ObservationPage> {
        (**self).page(scope, high_water, after, snapshot, limit)
    }
}
