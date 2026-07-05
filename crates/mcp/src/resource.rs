use serde_json::Value;

/// MCP resource definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceDefinition {
    /// Resource URI.
    pub uri: String,
    /// Resource name.
    pub name: String,
    /// Optional display title.
    pub title: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Optional MIME type.
    pub mime_type: Option<String>,
    /// Optional size.
    pub size: Option<u64>,
    /// Optional annotations.
    pub annotations: Option<Value>,
    /// Optional metadata.
    pub meta: Option<Value>,
}

/// MCP resource template definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceTemplateDefinition {
    /// URI template.
    pub uri_template: String,
    /// Template name.
    pub name: String,
    /// Optional display title.
    pub title: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Optional MIME type.
    pub mime_type: Option<String>,
    /// Optional annotations.
    pub annotations: Option<Value>,
    /// Optional metadata.
    pub meta: Option<Value>,
}

/// MCP completion result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCompletionResult {
    /// Completion values.
    pub values: Vec<String>,
    /// Optional total count.
    pub total: Option<u64>,
    /// Whether more values are available.
    pub has_more: bool,
}

/// Parse a resource definition.
pub fn parse_resource_definition(value: &Value) -> Option<McpResourceDefinition> {
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
    let annotations = value.get("annotations").cloned();
    let meta = value.get("_meta").cloned();
    Some(McpResourceDefinition {
        uri,
        name,
        title,
        description,
        mime_type,
        size,
        annotations,
        meta,
    })
}

/// Parse a resource template definition.
pub fn parse_resource_template_definition(value: &Value) -> Option<McpResourceTemplateDefinition> {
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
    let annotations = value.get("annotations").cloned();
    let meta = value.get("_meta").cloned();
    Some(McpResourceTemplateDefinition {
        uri_template,
        name,
        title,
        description,
        mime_type,
        annotations,
        meta,
    })
}

/// Parse a completion result.
#[must_use]
pub fn parse_completion_result(value: &Value) -> McpCompletionResult {
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
    /// Best available description text.
    #[must_use]
    pub fn description_text(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .or(self.mime_type.as_deref())
            .unwrap_or("")
    }
}

impl McpResourceTemplateDefinition {
    /// Best available description text.
    #[must_use]
    pub fn description_text(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .or(self.mime_type.as_deref())
            .unwrap_or("")
    }

    /// Return true if this template can match the URI.
    #[must_use]
    pub fn matches_uri(&self, uri: &str) -> bool {
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

/// Format a resource result into plain text.
#[must_use]
pub fn format_resource_result(result: &Value) -> String {
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
