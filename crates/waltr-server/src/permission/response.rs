//! Typed live permission responses. Approval state is private to the server policy.

use super::PermissionPattern;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionResponse {
    AllowOnce,
    AllowAlways {
        patterns: Vec<PermissionPattern>,
    },
    Deny {
        message: Option<String>,
        #[serde(default)]
        mode: PermissionDenyMode,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDenyMode {
    #[default]
    ReturnToolResult,
    CancelWork,
}
