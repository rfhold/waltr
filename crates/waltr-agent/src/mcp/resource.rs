use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpResourceDefinition {
    pub(crate) uri: String,
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) mime_type: Option<String>,
    pub(crate) size: Option<u64>,
}

pub(in crate::mcp) fn parse_resource_definition(value: &Value) -> Option<McpResourceDefinition> {
    let uri = value.get("uri")?.as_str()?.to_owned();
    let name = value.get("name")?.as_str()?.to_owned();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mime_type = value
        .get("mimeType")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let size = value.get("size").and_then(Value::as_u64);
    Some(McpResourceDefinition {
        uri,
        name,
        title,
        description,
        mime_type,
        size,
    })
}

impl McpResourceDefinition {
    pub(crate) fn description_text(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .or(self.mime_type.as_deref())
            .unwrap_or("")
    }
}

pub(in crate::mcp) fn format_resource_result(result: &Value) -> String {
    let mut parts = Vec::new();
    for content in result
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let uri = content
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if let Some(text) = content.get("text").and_then(Value::as_str) {
            parts.push(format!("{uri}:\n{text}"));
        } else if content.get("blob").and_then(Value::as_str).is_some() {
            let mime_type = content
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            parts.push(format!(
                "{uri}: [unsupported MCP resource blob content: {mime_type}]"
            ));
        }
    }
    if parts.is_empty() {
        result.to_string()
    } else {
        parts.join("\n")
    }
}
