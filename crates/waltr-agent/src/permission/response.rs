//! Typed host responses for permission requests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::PermissionPattern;
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct PermissionRuntimeState {
    #[serde(default)]
    pub(crate) one_shot: Vec<PermissionOneShotApproval>,
    #[serde(default)]
    pub(crate) patterns: Vec<PermissionPattern>,
}

impl PermissionRuntimeState {
    pub(crate) fn allow_once(
        &mut self,
        request_id: &ExternalRequestId,
        tool_call_id: impl Into<String>,
    ) {
        let approval = PermissionOneShotApproval {
            request_id: request_id.as_str().to_owned(),
            tool_call_id: tool_call_id.into(),
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

    pub(crate) fn consume_one_shot(&mut self, tool_call_id: &str) -> bool {
        let Some(index) = self
            .one_shot
            .iter()
            .position(|approval| approval.tool_call_id == tool_call_id)
        else {
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
}

fn permission_response_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "permission-response",
        message: message.into(),
    }
}
