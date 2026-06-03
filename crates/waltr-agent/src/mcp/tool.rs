use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mcp) struct McpToolDefinition {
    pub(in crate::mcp) name: String,
    pub(in crate::mcp) description: String,
    pub(in crate::mcp) input_schema: Value,
}

pub(in crate::mcp) fn parse_tool_definition(value: &Value) -> Option<McpToolDefinition> {
    let name = value.get("name")?.as_str()?.to_owned();
    let description = value
        .get("description")
        .or_else(|| value.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let input_schema = value
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(empty_object_schema);
    Some(McpToolDefinition {
        name,
        description,
        input_schema,
    })
}

pub(in crate::mcp) fn format_tool_result(result: &Value) -> String {
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
