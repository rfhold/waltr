use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpPromptDefinition {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpPromptArgument {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) required: bool,
}

pub(in crate::mcp) fn parse_prompt_definition(value: &Value) -> Option<McpPromptDefinition> {
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
    Some(McpPromptDefinition {
        name,
        title,
        description,
        arguments,
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
    pub(crate) fn description_text(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("")
    }
}

pub(in crate::mcp) fn format_prompt_result(result: &Value) -> String {
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
