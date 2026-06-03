//! Permission handler types.

use std::sync::Arc;

use super::{PermissionPattern, PermissionRequest};

/// Permission handler decision for an asked permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allow this call only.
    AllowOnce,
    /// Allow this call and store reusable patterns for the current run.
    AllowAlways(Vec<PermissionPattern>),
    /// Deny with a message.
    Deny(String),
}

/// Callback used to answer permission requests.
pub type PermissionHandler = dyn Fn(&PermissionRequest) -> PermissionDecision + Send + Sync;

/// Shared permission handler.
pub type SharedPermissionHandler = Arc<PermissionHandler>;
