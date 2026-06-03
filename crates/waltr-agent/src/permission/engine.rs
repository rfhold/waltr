//! Permission evaluation engine.

use std::fmt;

use crate::{LlmError, Result, ToolCall};

use super::{
    PermissionClaim, PermissionDecision, PermissionLevel, PermissionPattern, PermissionPolicy,
    PermissionRequest, SharedPermissionHandler, denial_message, policy_denial_message,
};

/// Run-scoped permission engine.
#[derive(Clone)]
pub struct PermissionEngine {
    patterns: Vec<PermissionPattern>,
    policy: PermissionPolicy,
    handler: Option<SharedPermissionHandler>,
}

impl PermissionEngine {
    /// Create a permission engine.
    #[must_use]
    pub fn new(policy: PermissionPolicy, handler: Option<SharedPermissionHandler>) -> Self {
        Self {
            patterns: Vec::new(),
            policy,
            handler,
        }
    }

    /// Return stored reusable patterns.
    #[must_use]
    pub fn patterns(&self) -> &[PermissionPattern] {
        &self.patterns
    }

    /// Return true if an ask handler is configured.
    #[must_use]
    pub const fn has_handler(&self) -> bool {
        self.handler.is_some()
    }

    /// Check claims for a tool call, mutating run-scoped approvals as needed.
    ///
    /// # Errors
    ///
    /// Returns an error when a claim is denied, when a claim requires `Ask` but no handler is
    /// configured, or when the handler denies the request.
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

        let Some(handler) = &self.handler else {
            return Err(permission_error(denial_message(
                "permission denied: no permission handler configured",
                &ask,
            )));
        };
        let request = PermissionRequest {
            tool_call: tool_call.clone(),
            claims: ask.clone(),
            summary: ask
                .iter()
                .map(|claim| claim.summary.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        };
        match handler(&request) {
            PermissionDecision::AllowOnce => Ok(()),
            PermissionDecision::AllowAlways(patterns) => {
                self.patterns.extend(patterns);
                Ok(())
            }
            PermissionDecision::Deny(message) => Err(permission_error(denial_message(
                &format!("permission denied: {message}"),
                &ask,
            ))),
        }
    }
}

impl fmt::Debug for PermissionEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionEngine")
            .field("patterns", &self.patterns)
            .field("policy", &self.policy)
            .field("has_handler", &self.handler.is_some())
            .finish()
    }
}

fn permission_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "permission",
        message: message.into(),
    }
}
