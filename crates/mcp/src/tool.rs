use serde_json::Value;

use crate::types::{McpIcon, parse_icons};

/// MCP tool definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolDefinition {
    /// Tool name.
    pub name: String,
    /// Optional display title.
    pub title: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Optional icon metadata.
    pub icons: Vec<McpIcon>,
    /// JSON schema for input.
    pub input_schema: Value,
    /// Optional JSON schema for output.
    pub output_schema: Option<Value>,
    /// Optional MCP annotations.
    pub annotations: Option<Value>,
    /// Optional extension metadata.
    pub meta: Option<Value>,
}

/// Parse a tool definition.
pub fn parse_tool_definition(value: &Value) -> Option<McpToolDefinition> {
    let name = value.get("name")?.as_str()?.to_owned();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let input_schema = value.get("inputSchema")?.clone();
    let icons = parse_icons(value);
    if value
        .get("icons")
        .and_then(Value::as_array)
        .is_some_and(|raw| raw.len() != icons.len())
    {
        return None;
    }
    Some(McpToolDefinition {
        name,
        title,
        description,
        icons,
        input_schema,
        output_schema: value.get("outputSchema").cloned(),
        annotations: value.get("annotations").cloned(),
        meta: value.get("_meta").cloned(),
    })
}

impl McpToolDefinition {
    /// Best available description text.
    #[must_use]
    pub fn description_text(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("")
    }
}

/// Format a tool result into plain text.
#[must_use]
pub fn format_tool_result(result: &Value) -> String {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts = Vec::new();
    for content in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = content.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some(other) => parts.push(format!("[unsupported MCP content type: {other}]")),
            None => {}
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        parts.push(format!("structuredContent: {structured}"));
    }
    let content = if parts.is_empty() {
        result.to_string()
    } else {
        parts.join("\n")
    };
    if is_error {
        format!("MCP tool error: {content}")
    } else {
        content
    }
}
