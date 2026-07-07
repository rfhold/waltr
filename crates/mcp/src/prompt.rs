use serde_json::Value;

use crate::types::{McpIcon, parse_icons};

/// MCP prompt definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptDefinition {
    /// Prompt name.
    pub name: String,
    /// Optional display title.
    pub title: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Optional icon metadata.
    pub icons: Vec<McpIcon>,
    /// Prompt arguments.
    pub arguments: Vec<McpPromptArgument>,
    /// Optional metadata.
    pub meta: Option<Value>,
}

/// MCP prompt argument metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptArgument {
    /// Argument name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Whether the argument is required.
    pub required: bool,
}

/// Parse a prompt definition.
pub fn parse_prompt_definition(value: &Value) -> Option<McpPromptDefinition> {
    let name = value.get("name")?.as_str()?.to_owned();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let arguments = value
        .get("arguments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_prompt_argument)
        .collect();
    let meta = value.get("_meta").cloned();
    Some(McpPromptDefinition {
        name,
        title,
        description,
        icons: parse_icons(value),
        arguments,
        meta,
    })
}

fn parse_prompt_argument(value: &Value) -> Option<McpPromptArgument> {
    let name = value.get("name")?.as_str()?.to_owned();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let required = value
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(McpPromptArgument {
        name,
        description,
        required,
    })
}

impl McpPromptDefinition {
    /// Best available description text.
    #[must_use]
    pub fn description_text(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("")
    }
}

/// Format a prompt result into plain text.
#[must_use]
pub fn format_prompt_result(result: &Value) -> String {
    let mut parts = Vec::new();
    for message in result
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = message.get("role").and_then(Value::as_str);
        let content = format_prompt_content(message.get("content"));
        if content.is_empty() {
            continue;
        }
        if let Some(role) = role {
            parts.push(format!("{role}: {content}"));
        } else {
            parts.push(content);
        }
    }
    if parts.is_empty() {
        result.to_string()
    } else {
        parts.join("\n")
    }
}

fn format_prompt_content(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content.get("type").and_then(Value::as_str) {
        Some("text") => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some(other) => format!("[unsupported MCP prompt content type: {other}]"),
        None => String::new(),
    }
}
