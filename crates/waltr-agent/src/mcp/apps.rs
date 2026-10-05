//! Durable MCP Apps provenance, deliberately separate from provider tool results.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Metadata key on an executed tool-result timeline event.
pub const MCP_APP_ASSOCIATION: &str = "mcp_app";

/// Versioned original execution snapshot for an associated MCP app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpAppAssociation {
    /// Snapshot schema version. Unknown versions must not confer authority.
    pub version: u32,
    /// Exact host-assigned source server, never parsed from a generated tool name.
    pub server: String,
    /// Exact MCP tool name.
    pub tool: String,
    /// Linked UI resource URI.
    pub resource_uri: String,
    /// Opaque stamp of the configured connection and non-secret authorization incarnation.
    pub source: String,
    /// Authority-relevant MCP tool declaration used by the execution.
    pub declaration: Value,
    /// Actual arguments after before-tool middleware.
    pub arguments: Value,
    /// Complete original MCP result, including structuredContent and _meta.
    pub result: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct McpAppSource {
    pub server: String,
    pub tool: String,
    pub source: String,
    pub declaration: Value,
    pub ui: mcp::apps::UiToolMeta,
}

pub(crate) type AppCapture = std::sync::Arc<std::sync::Mutex<Option<McpAppAssociation>>>;

pub(crate) fn unmodified_association(
    association: Option<McpAppAssociation>,
    original: &crate::ToolResult,
    projected: &crate::ToolResult,
) -> Option<McpAppAssociation> {
    association.filter(|_| original == projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_apps_redacted_projection_never_preserves_unredacted_association() {
        let association = McpAppAssociation {
            version: 1,
            server: "server".into(),
            tool: "tool".into(),
            resource_uri: "ui://app".into(),
            source: "authority".into(),
            declaration: json!({}),
            arguments: json!({}),
            result: json!({"content":[{"type":"text","text":"secret"}],"structuredContent":{"secret":true},"_meta":{"private":"secret"}}),
        };
        let original = crate::ToolResult::new("id", "tool", "secret");
        assert_eq!(
            unmodified_association(Some(association.clone()), &original, &original),
            Some(association.clone())
        );
        let mut redacted = original.clone();
        redacted.content = crate::ToolResultContent::text("redacted");
        assert!(unmodified_association(Some(association.clone()), &original, &redacted).is_none());
        let mut error = original.clone();
        error.is_error = true;
        assert!(unmodified_association(Some(association), &original, &error).is_none());
    }
}
