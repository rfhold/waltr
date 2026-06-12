//! Permission evaluation engine.

use std::fmt;

use crate::{LlmError, Result, ToolCall};

use super::{
    PermissionClaim, PermissionLevel, PermissionPattern, PermissionPolicy, policy_denial_message,
};

/// Run-scoped permission engine.
#[derive(Clone)]
pub struct PermissionEngine {
    patterns: Vec<PermissionPattern>,
    policy: PermissionPolicy,
}

impl PermissionEngine {
    /// Create a permission engine.
    #[must_use]
    pub const fn new(policy: PermissionPolicy) -> Self {
        Self {
            patterns: Vec::new(),
            policy,
        }
    }

    /// Return stored reusable patterns.
    #[must_use]
    pub fn patterns(&self) -> &[PermissionPattern] {
        &self.patterns
    }

    /// Check claims for a tool call.
    ///
    /// # Errors
    ///
    /// Returns an error when a claim is denied or requires a durable external permission response.
    pub fn check(&mut self, tool_call: &ToolCall, claims: &[PermissionClaim]) -> Result<()> {
        let unmatched = claims
            .iter()
            .filter(|claim| !self.patterns.iter().any(|pattern| pattern.matches(claim)))
            .cloned()
            .collect::<Vec<_>>();

        if unmatched.is_empty() {
            return Ok(());
        }

        let denied = unmatched
            .iter()
            .filter_map(|claim| {
                let effect = self.policy.evaluate_claim_effect(&tool_call.name, claim);
                (effect.level == PermissionLevel::Deny).then(|| (claim.clone(), effect))
            })
            .collect::<Vec<_>>();
        if !denied.is_empty() {
            return Err(permission_error(policy_denial_message(&denied)));
        }

        let ask = unmatched
            .iter()
            .filter(|claim| {
                self.policy
                    .evaluate_claim_effect(&tool_call.name, claim)
                    .level
                    == PermissionLevel::Ask
            })
            .cloned()
            .collect::<Vec<_>>();
        if ask.is_empty() {
            return Ok(());
        }

        Err(permission_error(policy_denial_message(
            &ask.into_iter()
                .map(|claim| (claim, super::PermissionEffect::ask()))
                .collect::<Vec<_>>(),
        )))
    }
}

impl fmt::Debug for PermissionEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionEngine")
            .field("patterns", &self.patterns)
            .field("policy", &self.policy)
            .finish()
    }
}

fn permission_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "permission",
        message: message.into(),
    }
}
