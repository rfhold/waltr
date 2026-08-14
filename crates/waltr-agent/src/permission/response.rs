//! Typed host responses for permission requests.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{PermissionClaim, PermissionOperation, PermissionPattern, PermissionResource};
use crate::{ExternalRequestId, LlmError, Result};

#[allow(clippy::redundant_pub_crate)]
pub(crate) const PERMISSION_STATE_NAMESPACE: &str = "permissions";

/// Host decision for a pending permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionResponse {
    /// Allow this pending tool call once.
    AllowOnce,
    /// Allow this request and future matching permission claims.
    AllowAlways {
        /// Reusable approval patterns to persist for this session.
        patterns: Vec<PermissionPattern>,
    },
    /// Deny the pending tool call.
    Deny {
        /// Optional user-facing denial message.
        message: Option<String>,
        /// How the denial should be applied to pending work.
        #[serde(default)]
        mode: PermissionDenyMode,
    },
}

impl PermissionResponse {
    /// Parse a permission response from a provider-neutral JSON payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload does not match a supported permission response shape.
    pub fn from_value(value: Value) -> Result<Self> {
        if let Some(value) = value.as_str() {
            return match value {
                "allow-once" | "allow_once" => Ok(Self::AllowOnce),
                "deny" => Ok(Self::Deny {
                    message: None,
                    mode: PermissionDenyMode::ReturnToolResult,
                }),
                _ => Err(permission_response_error(format!(
                    "unsupported permission response: {value}"
                ))),
            };
        }

        serde_json::from_value(value).map_err(|error| permission_response_error(error.to_string()))
    }
}

/// Denial handling for a pending permission request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDenyMode {
    /// Return a denial result to the model for the original tool call.
    #[default]
    ReturnToolResult,
    /// Cancel pending work without returning a tool result.
    CancelWork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct PermissionRuntimeState {
    pub(crate) version: u8,
    pub(crate) one_shot: Vec<PermissionOneShotApproval>,
    pub(crate) patterns: Vec<PermissionPattern>,
}

impl Default for PermissionRuntimeState {
    fn default() -> Self {
        Self {
            version: 2,
            one_shot: Vec::new(),
            patterns: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for PermissionRuntimeState {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredState {
            #[serde(default)]
            version: Option<u8>,
            #[serde(default)]
            one_shot: Vec<PermissionOneShotApproval>,
            #[serde(default)]
            patterns: Vec<PermissionPattern>,
        }

        let stored = StoredState::deserialize(deserializer)?;
        match stored.version {
            None | Some(1 | 2) => Ok(Self {
                version: 2,
                one_shot: stored.one_shot,
                patterns: stored.patterns,
            }),
            Some(version) => Err(serde::de::Error::custom(format!(
                "unsupported permission state version {version}"
            ))),
        }
    }
}

impl PermissionRuntimeState {
    pub(crate) fn allow_once(
        &mut self,
        request_id: &ExternalRequestId,
        tool_call_id: impl Into<String>,
        stage: PermissionApprovalStage,
        claims: Vec<PermissionClaimIdentity>,
    ) {
        let approval = PermissionOneShotApproval {
            request_id: request_id.as_str().to_owned(),
            tool_call_id: tool_call_id.into(),
            stage: Some(stage),
            claims: Some(claims),
        };
        if !self.one_shot.contains(&approval) {
            self.one_shot.push(approval);
        }
    }

    pub(crate) fn allow_patterns(&mut self, patterns: impl IntoIterator<Item = PermissionPattern>) {
        for pattern in patterns {
            if !self.patterns.contains(&pattern) {
                self.patterns.push(pattern);
            }
        }
    }

    pub(crate) fn has_one_shot(
        &self,
        request_id: &str,
        tool_call_id: &str,
        stage: PermissionApprovalStage,
        claims: &[PermissionClaimIdentity],
    ) -> bool {
        self.one_shot.iter().any(|approval| {
            approval.request_id == request_id
                && approval.tool_call_id == tool_call_id
                && approval.stage == Some(stage)
                && approval.claims.as_deref() == Some(claims)
        })
    }

    pub(crate) fn consume_one_shot(
        &mut self,
        request_id: &str,
        tool_call_id: &str,
        stage: PermissionApprovalStage,
        claims: &[PermissionClaimIdentity],
    ) -> bool {
        let Some(index) = self.one_shot.iter().position(|approval| {
            approval.request_id == request_id
                && approval.tool_call_id == tool_call_id
                && approval.stage == Some(stage)
                && approval.claims.as_deref() == Some(claims)
        }) else {
            return false;
        };
        self.one_shot.remove(index);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct PermissionOneShotApproval {
    pub(crate) request_id: String,
    pub(crate) tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stage: Option<PermissionApprovalStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) claims: Option<Vec<PermissionClaimIdentity>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionApprovalStage {
    WholeTool,
    Contextual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionClaimIdentity {
    operation: PermissionOperation,
    resource: PermissionResource,
}

pub fn permission_claim_identities(claims: &[PermissionClaim]) -> Vec<PermissionClaimIdentity> {
    let mut identities = claims
        .iter()
        .map(|claim| PermissionClaimIdentity {
            operation: claim.operation,
            resource: claim.resource.clone(),
        })
        .collect::<Vec<_>>();
    identities.sort_by_cached_key(|identity| {
        serde_json::to_string(identity).expect("permission claim identity serializes")
    });
    identities.dedup();
    identities
}

fn permission_response_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "permission-response",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn permission_state_writes_v2_and_reads_legacy_v1_and_v2() {
        let state = PermissionRuntimeState::default();
        assert_eq!(serde_json::to_value(&state).unwrap()["version"], 2);
        for value in [
            json!({ "one_shot": [], "patterns": [] }),
            json!({ "version": 1, "one_shot": [], "patterns": [] }),
            json!({ "version": 2, "one_shot": [], "patterns": [] }),
        ] {
            let state: PermissionRuntimeState = serde_json::from_value(value).unwrap();
            assert_eq!(state.version, 2);
        }
    }

    #[test]
    fn permission_state_rejects_unknown_versions() {
        let error = serde_json::from_value::<PermissionRuntimeState>(json!({
            "version": 3,
            "one_shot": [],
            "patterns": []
        }))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported permission state version 3")
        );
    }

    #[test]
    fn contextual_one_shot_requires_exact_request_and_claim_identity() {
        let request = ExternalRequestId::new("permission:request-a");
        let claim_a = PermissionClaim::new(
            PermissionOperation::Execute,
            PermissionResource::ShellCommand("command-a".to_owned()),
            "summary is excluded",
        );
        let claim_b = PermissionClaim::new(
            PermissionOperation::Execute,
            PermissionResource::ShellCommand("command-b".to_owned()),
            "different summary",
        );
        let identities_a = permission_claim_identities(&[claim_a]);
        let identities_b = permission_claim_identities(&[claim_b]);
        let mut state = PermissionRuntimeState::default();
        state.allow_once(
            &request,
            "call",
            PermissionApprovalStage::Contextual,
            identities_a.clone(),
        );

        assert!(!state.consume_one_shot(
            "permission:request-b",
            "call",
            PermissionApprovalStage::Contextual,
            &identities_a,
        ));
        assert!(!state.consume_one_shot(
            request.as_str(),
            "call",
            PermissionApprovalStage::Contextual,
            &identities_b,
        ));
        assert!(state.consume_one_shot(
            request.as_str(),
            "call",
            PermissionApprovalStage::Contextual,
            &identities_a,
        ));
    }

    #[test]
    fn legacy_one_shot_cannot_authorize_contextual_claims() {
        let mut state: PermissionRuntimeState = serde_json::from_value(json!({
            "one_shot": [{ "request_id": "legacy", "tool_call_id": "call" }],
            "patterns": []
        }))
        .unwrap();
        let identities = permission_claim_identities(&[PermissionClaim::new(
            PermissionOperation::Execute,
            PermissionResource::ShellCommand("command".to_owned()),
            "summary",
        )]);

        assert!(!state.consume_one_shot(
            "legacy",
            "call",
            PermissionApprovalStage::Contextual,
            &identities,
        ));
        assert_eq!(state.one_shot.len(), 1);
    }
}
