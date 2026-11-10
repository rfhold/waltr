//! Durable, non-replayable association for sessionless capability effects.

use std::{future::Future, pin::Pin};

/// Exact host-admitted identity and declaration, never a replay instruction.
#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ExternalDispatch {
    /// Server-generated, single-use operation identity.
    pub operation_id: String,
    /// Authenticated identity and scopes, without credentials or bearer tokens.
    pub principal: serde_json::Value,
    /// Exact concrete target identity, independent of its visible alias.
    pub identity: String,
    /// Immutable dispatcher declaration revision.
    pub source_revision: String,
    /// Exact selected tool schema and declaration.
    pub declaration: serde_json::Value,
    /// Original admitted call and arguments.
    pub call: waltr_agent::ToolCall,
}

impl std::fmt::Debug for ExternalDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalDispatch").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Sanitized journal failures; never include arguments or result payloads.
pub enum JournalError {
    /// Persistence did not acknowledge the operation.
    Unavailable,
    /// The identity exists or the exact settlement CAS failed.
    Conflict,
    /// The association is malformed.
    Invalid,
}

/// Owned persistence operation.
pub type JournalFuture<T> = Pin<Box<dyn Future<Output = Result<T, JournalError>> + Send>>;

/// An acknowledged association is mandatory before Prepare or any concrete effect.
/// Completion is a single exact CAS, only for a known handler result. Errors never retry it.
pub trait ExternalDispatchStore: Send + Sync {
    /// Insert a new uncertain association; existing identities always conflict.
    fn associate(&self, dispatch: ExternalDispatch) -> JournalFuture<()>;
    /// Persist a known result only for the exact still-unsettled association.
    fn complete(&self, dispatch: ExternalDispatch, outcome: serde_json::Value)
    -> JournalFuture<()>;
}
