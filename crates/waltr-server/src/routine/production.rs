//! Shared application-owned JavaScript execution and authenticated host composition.
use super::{code_mode::CodeProfile, ports::PortError};
use crate::{agent_host::authorized_tools::ConcreteToolTarget, permission::PermissionPolicy};
use std::collections::{BTreeMap, BTreeSet};
use waltr_agent::SessionId;

mod runtime;
pub use runtime::{ProductionInput, ProductionRuntime};
pub mod standalone;

/// Operator-selected authority for Routine execution.
#[derive(Clone, Debug, Default)]
pub struct RoutineAuthority {
    /// Exact identities, durable sessions, and independently restricted code profiles.
    pub code: Vec<RoutineCodeGrant>,
    /// Exact normal-tool identities available to installed Routine programs.
    pub targets: BTreeMap<String, ConcreteToolTarget>,
    /// Explicit speech provider/model selections; no ambient provider fallback.
    pub recognition: Option<(String, String)>,
    /// Exact diarized snapshot selection; absent retains explicit legacy recognition support.
    pub recognition_snapshot: Option<(String, String)>,
    /// Explicit synthesis provider/model selection.
    pub synthesis: Option<(String, String)>,
    /// Exact provider/model native-input ceilings; absence denies attachments.
    pub helper_inputs: Vec<HelperInputAuthority>,
    /// Allowed per-call synthesis choices for the pinned backend.
    pub synthesis_choices: Vec<super::media::speech::SynthesisSelection>,
    /// Explicit normal-tool ceiling for media helpers; absence grants no tools.
    pub media_restriction: Option<PermissionPolicy>,
}

/// Native input facts for one exact inference identity, never a helper/task profile.
#[derive(Clone, Debug)]
pub struct HelperInputAuthority {
    /// Exact inference provider key.
    pub provider: String,
    /// Exact inference model identifier.
    pub model: String,
    /// Host-approved model input support and encoded-body ceiling.
    pub capabilities: waltr_agent::TransientModelCapabilities,
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
        for (provider, model) in self.recognition_snapshot.iter() {
            if [provider, model].iter().any(|value| {
                value.is_empty()
                    || value.len() > 256
                    || value.trim() != value.as_str()
                    || value.chars().any(char::is_control)
            }) {
                return Err(PortError::Invalid);
            }
        }
        if self.code.len() > 128
            || self.targets.len() > 128
            || self.helper_inputs.len() > 128 || self.synthesis_choices.len() > 128
            || self.targets.keys().any(|key| {
                !key.strip_prefix("tool.")
                    .is_some_and(super::definition::identifier)
            })
        {
            return Err(PortError::Invalid);
        }
        let mut inputs = BTreeSet::new();
        for input in &self.helper_inputs {
            if input.provider.is_empty() || input.provider.len() > 256 || input.model.is_empty() || input.model.len() > 256
                || !inputs.insert((&input.provider, &input.model))
                || !(1..=32*1024*1024).contains(&input.capabilities.max_body_bytes) {
                return Err(PortError::Invalid);
            }
        }
        if self.synthesis_choices.iter().any(|choice| choice.validate().is_err()) { return Err(PortError::Invalid); }
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
