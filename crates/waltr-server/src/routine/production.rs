//! Shared application-owned JavaScript execution and authenticated host composition.
use super::{code_mode::CodeProfile, ports::PortError};
use crate::agent_host::authorized_tools::ConcreteToolTarget;
use std::collections::{BTreeMap, BTreeSet};
use waltr_agent::SessionId;

mod runtime;
pub use runtime::{ProductionInput, ProductionRuntime};

/// Operator-selected authority for Routine execution.
#[derive(Clone, Debug, Default)]
pub struct RoutineAuthority {
    /// Exact identities, durable sessions, and independently restricted code profiles.
    pub code: Vec<RoutineCodeGrant>,
    /// Exact normal-tool identities available to installed Routine programs.
    pub targets: BTreeMap<String, ConcreteToolTarget>,
}

/// Code grants are never inferred from application membership or model visibility.
#[derive(Clone, Debug)]
pub struct RoutineCodeGrant {
    /// Validated access issuer.
    pub issuer: String,
    /// Validated access subject.
    pub subject: String,
    /// Exact durable sessions selected by the operator.
    pub sessions: BTreeSet<SessionId>,
    /// Exact tools and named helpers, intersected with the normal agent policy.
    pub profile: CodeProfile,
    /// Optional host-selected session for public MCP code; must belong to `sessions`.
    pub public_mcp_session: Option<SessionId>,
}

impl RoutineAuthority {
    pub(crate) fn validate(&self) -> Result<(), PortError> {
        if self.code.len() > 128
            || self.targets.len() > 128
            || self.targets.keys().any(|key| {
                !key.strip_prefix("tool.")
                    .is_some_and(super::definition::identifier)
            })
        {
            return Err(PortError::Invalid);
        }
        let mut identities = BTreeSet::new();
        for grant in &self.code {
            if !identity(&grant.issuer, &grant.subject)
                || grant.sessions.len() > 128
                || grant
                    .public_mcp_session
                    .as_ref()
                    .is_some_and(|id| !grant.sessions.contains(id))
                || !identities.insert((&grant.issuer, &grant.subject))
                || grant.profile.targets.len() > 128
                || grant.profile.helpers.len() > 32
                || grant.profile.targets.keys().any(|key| {
                    !key.strip_prefix("tool.")
                        .is_some_and(super::definition::identifier)
                })
                || grant
                    .profile
                    .helpers
                    .iter()
                    .any(|name| !super::definition::identifier(name))
            {
                return Err(PortError::Invalid);
            }
        }
        Ok(())
    }
}
fn identity(issuer: &str, subject: &str) -> bool {
    !issuer.is_empty() && issuer.len() <= 512 && !subject.is_empty() && subject.len() <= 512
}

/// Shared JavaScript execution and catalog coordination readiness.
#[derive(Debug, Clone)]
pub enum RuntimeHealth {
    /// Admission and catalog coordination are available.
    Running,
    /// Coordination stopped or an owned task failed.
    Failed(String),
}
