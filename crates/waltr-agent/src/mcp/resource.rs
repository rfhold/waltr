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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpResourceTemplateDefinition {
    pub(crate) uri_template: String,
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpCompletionResult {
    pub(crate) values: Vec<String>,
    pub(crate) total: Option<u64>,
    pub(crate) has_more: bool,
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

pub(in crate::mcp) fn parse_resource_template_definition(
    value: &Value,
) -> Option<McpResourceTemplateDefinition> {
    let uri_template = value.get("uriTemplate")?.as_str()?.to_owned();
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
    Some(McpResourceTemplateDefinition {
        uri_template,
        name,
        title,
        description,
        mime_type,
    })
}

pub(in crate::mcp) fn parse_completion_result(value: &Value) -> McpCompletionResult {
    let completion = value.get("completion").unwrap_or(value);
    let values = completion
        .get("values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect();
    let total = completion.get("total").and_then(Value::as_u64);
    let has_more = completion
        .get("hasMore")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    McpCompletionResult {
        values,
        total,
        has_more,
    }
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

impl McpResourceTemplateDefinition {
    pub(crate) fn description_text(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .or(self.mime_type.as_deref())
            .unwrap_or("")
    }

    pub(crate) fn matches_uri(&self, uri: &str) -> bool {
        uri_template_matches(&self.uri_template, uri)
    }
}

fn uri_template_matches(template: &str, uri: &str) -> bool {
    let mut remaining_uri = uri;
    let mut remaining_template = template;

    while let Some(open) = remaining_template.find('{') {
        let literal = &remaining_template[..open];
        if !remaining_uri.starts_with(literal) {
            return false;
        }
        remaining_uri = &remaining_uri[literal.len()..];

        let Some(close) = remaining_template[open + 1..].find('}') else {
            return false;
        };
        let after_variable = &remaining_template[open + close + 2..];
        if after_variable.is_empty() {
            return !remaining_uri.is_empty();
        }

        let next_literal_end = after_variable.find('{').unwrap_or(after_variable.len());
        let next_literal = &after_variable[..next_literal_end];
        if next_literal.is_empty() {
            remaining_template = after_variable;
            continue;
        }
        let Some(next_literal_start) = remaining_uri.find(next_literal) else {
            return false;
        };
        if next_literal_start == 0 {
            return false;
        }
        remaining_uri = &remaining_uri[next_literal_start..];
        remaining_template = after_variable;
    }

    remaining_uri == remaining_template
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
