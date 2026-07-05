use serde_json::{Value, json};

use crate::{
    Result,
    prompt::{McpPromptDefinition, format_prompt_result, parse_prompt_definition},
    resource::{
        McpCompletionResult, McpResourceDefinition, McpResourceTemplateDefinition,
        format_resource_result, parse_completion_result, parse_resource_definition,
        parse_resource_template_definition,
    },
    tool::{McpToolDefinition, format_tool_result, parse_tool_definition},
};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A root directory exposed by an MCP client.
pub struct McpRoot {
    /// Root URI.
    pub uri: String,
    /// Optional display name.
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
/// Progress notification received from an MCP server.
pub struct McpProgressNotification {
    /// Progress token associated with the request.
    pub token: Value,
    /// Current progress value.
    pub progress: Option<f64>,
    /// Optional total progress value.
    pub total: Option<f64>,
    /// Optional progress message.
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
/// Typed MCP tool call request.
pub struct McpToolCall {
    /// Tool name.
    pub name: String,
    /// Tool arguments object.
    pub arguments: Value,
    /// Optional progress token.
    pub progress_token: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
/// MCP tool result.
pub struct McpToolResult {
    /// Raw MCP result payload.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
/// MCP prompt result.
pub struct McpPromptResult {
    /// Raw MCP result payload.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
/// MCP resource read result.
pub struct McpResourceResult {
    /// Raw MCP result payload.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// MCP resource-template completion request.
pub struct McpCompletionRequest {
    /// Resource URI template.
    pub uri_template: String,
    /// Argument name to complete.
    pub argument_name: String,
    /// Current argument value.
    pub argument_value: String,
    /// Optional context arguments.
    pub context_arguments: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
/// Page of MCP tools.
pub struct McpToolList {
    /// Tools in this page.
    pub tools: Vec<McpToolDefinition>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
/// Page of MCP prompts.
pub struct McpPromptList {
    /// Prompts in this page.
    pub prompts: Vec<McpPromptDefinition>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
/// Page of MCP resources.
pub struct McpResourceList {
    /// Resources in this page.
    pub resources: Vec<McpResourceDefinition>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
/// Page of MCP resource templates.
pub struct McpResourceTemplateList {
    /// Resource templates in this page.
    pub resource_templates: Vec<McpResourceTemplateDefinition>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

impl McpRoot {
    /// Create a root from a URI and optional display name.
    #[must_use]
    pub fn new(uri: impl Into<String>, name: impl Into<Option<String>>) -> Self {
        Self {
            uri: uri.into(),
            name: name.into(),
        }
    }

    /// Convert this root to the MCP wire shape.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut value = json!({ "uri": self.uri });
        if let Some(name) = &self.name {
            value["name"] = json!(name);
        }
        value
    }
}

impl McpToolCall {
    /// Create a tool call.
    #[must_use]
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
            progress_token: None,
        }
    }

    /// Attach a progress token to the call.
    #[must_use]
    pub fn with_progress_token(mut self, token: impl Into<Value>) -> Self {
        self.progress_token = Some(token.into());
        self
    }

    /// Convert this call to MCP request params.
    #[must_use]
    pub fn to_params(&self) -> Value {
        let mut params = json!({ "name": self.name, "arguments": self.arguments });
        if let Some(token) = &self.progress_token {
            params["_meta"] = json!({ "progressToken": token });
        }
        params
    }

    /// Parse a tool call from MCP request params.
    pub fn from_params(value: &Value) -> std::result::Result<Self, String> {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing tool name".to_owned())?
            .to_owned();
        let arguments = value.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let progress_token = value
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .cloned();
        Ok(Self {
            name,
            arguments,
            progress_token,
        })
    }
}

impl McpToolResult {
    /// Wrap a raw MCP tool result.
    #[must_use]
    pub fn new(raw: Value) -> Self {
        Self { raw }
    }

    /// Create a text-only tool result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(json!({ "content": [{ "type": "text", "text": text.into() }] }))
    }

    /// Format supported result content as plain text.
    #[must_use]
    pub fn format_text(&self) -> String {
        format_tool_result(&self.raw)
    }
}

impl McpPromptResult {
    /// Wrap a raw MCP prompt result.
    #[must_use]
    pub fn new(raw: Value) -> Self {
        Self { raw }
    }

    /// Create a single user text prompt result.
    #[must_use]
    pub fn text_user(text: impl Into<String>) -> Self {
        Self::new(json!({
            "messages": [{ "role": "user", "content": { "type": "text", "text": text.into() } }]
        }))
    }

    /// Format supported result content as plain text.
    #[must_use]
    pub fn format_text(&self) -> String {
        format_prompt_result(&self.raw)
    }
}

impl McpResourceResult {
    /// Wrap a raw MCP resource result.
    #[must_use]
    pub fn new(raw: Value) -> Self {
        Self { raw }
    }

    /// Create a text resource result.
    #[must_use]
    pub fn text(
        uri: impl Into<String>,
        mime_type: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::new(json!({
            "contents": [{ "uri": uri.into(), "mimeType": mime_type.into(), "text": text.into() }]
        }))
    }

    /// Format supported result content as plain text.
    #[must_use]
    pub fn format_text(&self) -> String {
        format_resource_result(&self.raw)
    }
}

impl McpCompletionRequest {
    /// Create a resource-template completion request.
    #[must_use]
    pub fn new(
        uri_template: impl Into<String>,
        argument_name: impl Into<String>,
        argument_value: impl Into<String>,
    ) -> Self {
        Self {
            uri_template: uri_template.into(),
            argument_name: argument_name.into(),
            argument_value: argument_value.into(),
            context_arguments: serde_json::Map::new(),
        }
    }

    /// Convert this request to MCP request params.
    #[must_use]
    pub fn to_params(&self) -> Value {
        let mut params = json!({
            "ref": { "type": "ref/resource", "uri": self.uri_template },
            "argument": { "name": self.argument_name, "value": self.argument_value },
        });
        if !self.context_arguments.is_empty() {
            params["context"] = json!({ "arguments": self.context_arguments });
        }
        params
    }
}

impl McpToolList {
    /// Parse a tool-list page from an MCP response.
    pub fn parse(value: &Value) -> Result<Self> {
        let tools = value
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_tool_definition)
            .collect();
        Ok(Self {
            tools,
            next_cursor: next_cursor(value),
        })
    }

    /// Convert this page to the MCP wire shape.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut value =
            json!({ "tools": self.tools.iter().map(tool_to_value).collect::<Vec<_>>() });
        add_cursor(&mut value, &self.next_cursor);
        value
    }
}

impl McpPromptList {
    /// Parse a prompt-list page from an MCP response.
    pub fn parse(value: &Value) -> Result<Self> {
        let prompts = value
            .get("prompts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_prompt_definition)
            .collect();
        Ok(Self {
            prompts,
            next_cursor: next_cursor(value),
        })
    }

    /// Convert this page to the MCP wire shape.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut value =
            json!({ "prompts": self.prompts.iter().map(prompt_to_value).collect::<Vec<_>>() });
        add_cursor(&mut value, &self.next_cursor);
        value
    }
}

impl McpResourceList {
    /// Parse a resource-list page from an MCP response.
    pub fn parse(value: &Value) -> Result<Self> {
        let resources = value
            .get("resources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_resource_definition)
            .collect();
        Ok(Self {
            resources,
            next_cursor: next_cursor(value),
        })
    }

    /// Convert this page to the MCP wire shape.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut value = json!({ "resources": self.resources.iter().map(resource_to_value).collect::<Vec<_>>() });
        add_cursor(&mut value, &self.next_cursor);
        value
    }
}

impl McpResourceTemplateList {
    /// Parse a resource-template-list page from an MCP response.
    pub fn parse(value: &Value) -> Result<Self> {
        let resource_templates = value
            .get("resourceTemplates")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_resource_template_definition)
            .collect();
        Ok(Self {
            resource_templates,
            next_cursor: next_cursor(value),
        })
    }

    /// Convert this page to the MCP wire shape.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut value = json!({
            "resourceTemplates": self.resource_templates.iter().map(resource_template_to_value).collect::<Vec<_>>()
        });
        add_cursor(&mut value, &self.next_cursor);
        value
    }
}

/// Parse a completion result from an MCP response.
pub fn parse_completion(value: &Value) -> McpCompletionResult {
    parse_completion_result(value)
}

fn next_cursor(value: &Value) -> Option<String> {
    value
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn add_cursor(value: &mut Value, cursor: &Option<String>) {
    if let Some(cursor) = cursor {
        value["nextCursor"] = json!(cursor);
    }
}

fn tool_to_value(tool: &McpToolDefinition) -> Value {
    let mut value = json!({ "name": tool.name, "inputSchema": tool.input_schema });
    optional_string(&mut value, "title", &tool.title);
    optional_string(&mut value, "description", &tool.description);
    optional_value(&mut value, "outputSchema", &tool.output_schema);
    optional_value(&mut value, "annotations", &tool.annotations);
    value
}

fn prompt_to_value(prompt: &McpPromptDefinition) -> Value {
    let mut value = json!({ "name": prompt.name });
    optional_string(&mut value, "title", &prompt.title);
    optional_string(&mut value, "description", &prompt.description);
    if !prompt.arguments.is_empty() {
        value["arguments"] = json!(
            prompt
                .arguments
                .iter()
                .map(|argument| {
                    let mut value = json!({ "name": argument.name, "required": argument.required });
                    optional_string(&mut value, "description", &argument.description);
                    value
                })
                .collect::<Vec<_>>()
        );
    }
    optional_value(&mut value, "_meta", &prompt.meta);
    value
}

fn resource_to_value(resource: &McpResourceDefinition) -> Value {
    let mut value = json!({ "uri": resource.uri, "name": resource.name });
    optional_string(&mut value, "title", &resource.title);
    optional_string(&mut value, "description", &resource.description);
    optional_string(&mut value, "mimeType", &resource.mime_type);
    if let Some(size) = resource.size {
        value["size"] = json!(size);
    }
    optional_value(&mut value, "annotations", &resource.annotations);
    optional_value(&mut value, "_meta", &resource.meta);
    value
}

fn resource_template_to_value(resource_template: &McpResourceTemplateDefinition) -> Value {
    let mut value = json!({
        "uriTemplate": resource_template.uri_template,
        "name": resource_template.name,
    });
    optional_string(&mut value, "title", &resource_template.title);
    optional_string(&mut value, "description", &resource_template.description);
    optional_string(&mut value, "mimeType", &resource_template.mime_type);
    optional_value(&mut value, "annotations", &resource_template.annotations);
    optional_value(&mut value, "_meta", &resource_template.meta);
    value
}

fn optional_string(value: &mut Value, key: &str, item: &Option<String>) {
    if let Some(item) = item {
        value[key] = json!(item);
    }
}

fn optional_value(value: &mut Value, key: &str, item: &Option<Value>) {
    if let Some(item) = item {
        value[key] = item.clone();
    }
}

/// Require a field in a JSON object.
pub fn require_field<'a>(value: &'a Value, field: &str) -> std::result::Result<&'a Value, String> {
    value
        .get(field)
        .ok_or_else(|| format!("missing field `{field}`"))
}

/// Require a string field in a JSON object.
pub fn require_string(value: &Value, field: &str) -> std::result::Result<String, String> {
    require_field(value, field)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("field `{field}` must be a string"))
}
