use serde_json::{Value, json};

use crate::types::{McpIcon, parse_icons};

#[derive(Debug, Clone, PartialEq, Eq)]
/// MCP tool task support level.
pub enum McpToolTaskSupport {
    /// Tool calls must not be run as tasks.
    Forbidden,
    /// Tool calls may be run as tasks.
    Optional,
    /// Tool calls must be run as tasks.
    Required,
}

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
    /// Optional task execution support metadata.
    pub task_support: Option<McpToolTaskSupport>,
    /// Optional MCP annotations.
    pub annotations: Option<Value>,
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
    let input_schema = value
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(empty_object_schema);
    Some(McpToolDefinition {
        name,
        title,
        description,
        icons: parse_icons(value),
        input_schema,
        output_schema: value.get("outputSchema").cloned(),
        task_support: value
            .get("execution")
            .and_then(|execution| execution.get("taskSupport"))
            .and_then(Value::as_str)
            .and_then(McpToolTaskSupport::parse),
        annotations: value.get("annotations").cloned(),
    })
}

impl McpToolTaskSupport {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Forbidden => "forbidden",
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "forbidden" => Some(Self::Forbidden),
            "optional" => Some(Self::Optional),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
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

fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}
