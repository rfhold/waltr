//! Permission request types.

use crate::ToolCall;

use super::{PermissionClaim, PermissionEffect};

/// Permission check context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionCheckContext<'a> {
    /// Tool name being checked.
    pub tool_name: &'a str,
    /// Claims being checked.
    pub claims: &'a [PermissionClaim],
}

/// Permission request passed to an ask handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Tool call being checked.
    pub tool_call: ToolCall,
    /// Claims requiring a decision.
    pub claims: Vec<PermissionClaim>,
    /// Human-readable request summary.
    pub summary: String,
}

pub(in crate::permission) fn denial_message(prefix: &str, claims: &[PermissionClaim]) -> String {
    let summaries = claims
        .iter()
        .map(|claim| claim.summary.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    if summaries.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {summaries}")
    }
}

pub(in crate::permission) fn policy_denial_message(
    denied: &[(PermissionClaim, PermissionEffect)],
) -> String {
    let claims = denied
        .iter()
        .map(|(claim, _)| claim.clone())
        .collect::<Vec<_>>();
    let messages = denied
        .iter()
        .filter_map(|(_, effect)| effect.message.as_deref())
        .collect::<Vec<_>>();
    if messages.is_empty() {
        return denial_message("permission denied", &claims);
    }

    let mut unique_messages = Vec::new();
    for message in messages {
        if !unique_messages.contains(&message) {
            unique_messages.push(message);
        }
    }
    denial_message(
        &format!("permission denied: {}", unique_messages.join("; ")),
        &claims,
    )
}
