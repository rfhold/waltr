use serde_json::{Value, json};

use crate::{
    Error, Result,
    prompt::{McpPromptDefinition, format_prompt_result, parse_prompt_definition},
    resource::{
        McpCompletionResult, McpResourceDefinition, McpResourceTemplateDefinition,
        format_resource_result, parse_completion_result, parse_resource_definition,
        parse_resource_template_definition,
    },
    tool::{McpToolDefinition, format_tool_result, parse_tool_definition},
};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Icon metadata used by MCP implementations and feature definitions.
pub struct McpIcon {
    /// Icon source URI.
    pub src: String,
    /// Optional MIME type.
    pub mime_type: Option<String>,
    /// Optional icon size hint.
    pub sizes: Option<Vec<String>>,
    /// Optional theme hint.
    pub theme: Option<McpIconTheme>,
}

/// Theme in which an MCP icon is intended to be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpIconTheme {
    /// Icon intended for a light visual theme.
    Light,
    /// Icon intended for a dark visual theme.
    Dark,
}

impl McpIconTheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
/// Typed MCP tool call request.
pub struct McpToolCall {
    /// Tool name.
    pub name: String,
    /// Tool arguments object.
    pub arguments: Value,
    /// Optional progress token.
    pub progress_token: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// MCP tool result.
pub struct McpToolResult {
    /// Raw MCP result payload.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// MCP prompt result.
pub struct McpPromptResult {
    /// Raw MCP result payload.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// MCP resource read result.
pub struct McpResourceResult {
    /// Raw MCP result payload.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// MCP resource-template completion request.
pub struct McpCompletionRequest {
    /// Prompt or resource target being completed.
    pub reference: McpCompletionReference,
    /// Argument name to complete.
    pub argument_name: String,
    /// Current argument value.
    pub argument_value: String,
    /// Optional context arguments.
    pub context_arguments: serde_json::Map<String, Value>,
}

/// Target of an MCP completion request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCompletionReference {
    /// Resource URI template reference.
    Resource(String),
    /// Prompt-name reference.
    Prompt(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Page of MCP tools.
pub struct McpToolList {
    /// Tools in this page.
    pub tools: Vec<McpToolDefinition>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Page of MCP prompts.
pub struct McpPromptList {
    /// Prompts in this page.
    pub prompts: Vec<McpPromptDefinition>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Page of MCP resources.
pub struct McpResourceList {
    /// Resources in this page.
    pub resources: Vec<McpResourceDefinition>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

impl McpIcon {
    /// Create an icon from its source URI.
    #[must_use]
    pub fn new(src: impl Into<String>) -> Self {
        Self {
            src: src.into(),
            mime_type: None,
            sizes: None,
            theme: None,
        }
    }
}

pub fn parse_icons(value: &Value) -> Vec<McpIcon> {
    value
        .get("icons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_icon)
        .collect()
}

fn parse_icon(value: &Value) -> Option<McpIcon> {
    Some(McpIcon {
        src: value.get("src")?.as_str()?.to_owned(),
        mime_type: value
            .get("mimeType")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        sizes: value.get("sizes").and_then(Value::as_array).map(|sizes| {
            sizes
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        }),
        theme: match value.get("theme").and_then(Value::as_str) {
            Some("light") => Some(McpIconTheme::Light),
            Some("dark") => Some(McpIconTheme::Dark),
            Some(_) => return None,
            None => None,
        },
    })
}

pub fn icons_to_value(icons: &[McpIcon]) -> Value {
    Value::Array(icons.iter().map(icon_to_value).collect())
}

fn icon_to_value(icon: &McpIcon) -> Value {
    let mut value = json!({ "src": icon.src });
    optional_string(&mut value, "mimeType", icon.mime_type.as_ref());
    if let Some(sizes) = &icon.sizes {
        value["sizes"] = json!(sizes);
    }
    if let Some(theme) = icon.theme {
        value["theme"] = Value::String(theme.as_str().to_owned());
    }
    value
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
    ///
    /// # Errors
    ///
    /// Returns an error when the required tool name is absent or invalid.
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
        if progress_token
            .as_ref()
            .is_some_and(|token| !valid_progress_token(token))
        {
            return Err("progress token must be a string or integer".to_owned());
        }
        Ok(Self {
            name,
            arguments,
            progress_token,
        })
    }
}

pub fn valid_progress_token(token: &Value) -> bool {
    token.as_str().is_some() || token.as_i64().is_some()
}

impl McpToolResult {
    /// Wrap a raw MCP tool result.
    #[must_use]
    pub const fn new(raw: Value) -> Self {
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
    pub const fn new(raw: Value) -> Self {
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
    pub const fn new(raw: Value) -> Self {
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
            reference: McpCompletionReference::Resource(uri_template.into()),
            argument_name: argument_name.into(),
            argument_value: argument_value.into(),
            context_arguments: serde_json::Map::new(),
        }
    }

    /// Create a prompt completion request.
    #[must_use]
    pub fn for_prompt(
        prompt_name: impl Into<String>,
        argument_name: impl Into<String>,
        argument_value: impl Into<String>,
    ) -> Self {
        Self {
            reference: McpCompletionReference::Prompt(prompt_name.into()),
            argument_name: argument_name.into(),
            argument_value: argument_value.into(),
            context_arguments: serde_json::Map::new(),
        }
    }

    /// Convert this request to MCP request params.
    #[must_use]
    pub fn to_params(&self) -> Value {
        let reference = match &self.reference {
            McpCompletionReference::Resource(uri) => json!({ "type": "ref/resource", "uri": uri }),
            McpCompletionReference::Prompt(name) => json!({ "type": "ref/prompt", "name": name }),
        };
        let mut params = json!({
            "ref": reference,
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
    ///
    /// # Errors
    ///
    /// Returns an error when the response page cannot be parsed.
    pub fn parse(value: &Value) -> Result<Self> {
        let tools = value
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::protocol("MCP tools/list result omitted tools"))?
            .iter()
            .map(|tool| {
                parse_tool_definition(tool)
                    .ok_or_else(|| Error::protocol("invalid MCP tool definition"))
            })
            .collect::<Result<Vec<_>>>()?;
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
        add_cursor(&mut value, self.next_cursor.as_ref());
        value
    }
}

impl McpPromptList {
    /// Parse a prompt-list page from an MCP response.
    ///
    /// # Errors
    ///
    /// Returns an error when the response page cannot be parsed.
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
        add_cursor(&mut value, self.next_cursor.as_ref());
        value
    }
}

impl McpResourceList {
    /// Parse a resource-list page from an MCP response.
    ///
    /// # Errors
    ///
    /// Returns an error when the response page cannot be parsed.
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
        add_cursor(&mut value, self.next_cursor.as_ref());
        value
    }
}

impl McpResourceTemplateList {
    /// Parse a resource-template-list page from an MCP response.
    ///
    /// # Errors
    ///
    /// Returns an error when the response page cannot be parsed.
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
        add_cursor(&mut value, self.next_cursor.as_ref());
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

fn add_cursor(value: &mut Value, cursor: Option<&String>) {
    if let Some(cursor) = cursor {
        value["nextCursor"] = json!(cursor);
    }
}

fn tool_to_value(tool: &McpToolDefinition) -> Value {
    let mut value = json!({ "name": tool.name, "inputSchema": tool.input_schema });
    optional_string(&mut value, "title", tool.title.as_ref());
    optional_string(&mut value, "description", tool.description.as_ref());
    if !tool.icons.is_empty() {
        value["icons"] = icons_to_value(&tool.icons);
    }
    optional_value(&mut value, "outputSchema", tool.output_schema.as_ref());
    optional_value(&mut value, "annotations", tool.annotations.as_ref());
    optional_value(&mut value, "_meta", tool.meta.as_ref());
    value
}

fn prompt_to_value(prompt: &McpPromptDefinition) -> Value {
    let mut value = json!({ "name": prompt.name });
    optional_string(&mut value, "title", prompt.title.as_ref());
    optional_string(&mut value, "description", prompt.description.as_ref());
    if !prompt.icons.is_empty() {
        value["icons"] = icons_to_value(&prompt.icons);
    }
    if !prompt.arguments.is_empty() {
        value["arguments"] = json!(
            prompt
                .arguments
                .iter()
                .map(|argument| {
                    let mut value = json!({ "name": argument.name, "required": argument.required });
                    optional_string(&mut value, "title", argument.title.as_ref());
                    optional_string(&mut value, "description", argument.description.as_ref());
                    value
                })
                .collect::<Vec<_>>()
        );
    }
    optional_value(&mut value, "_meta", prompt.meta.as_ref());
    value
}

fn resource_to_value(resource: &McpResourceDefinition) -> Value {
    let mut value = json!({ "uri": resource.uri, "name": resource.name });
    optional_string(&mut value, "title", resource.title.as_ref());
    optional_string(&mut value, "description", resource.description.as_ref());
    optional_string(&mut value, "mimeType", resource.mime_type.as_ref());
    if !resource.icons.is_empty() {
        value["icons"] = icons_to_value(&resource.icons);
    }
    if let Some(size) = resource.size {
        value["size"] = json!(size);
    }
    optional_value(&mut value, "annotations", resource.annotations.as_ref());
    optional_value(&mut value, "_meta", resource.meta.as_ref());
    value
}

fn resource_template_to_value(resource_template: &McpResourceTemplateDefinition) -> Value {
    let mut value = json!({
        "uriTemplate": resource_template.uri_template,
        "name": resource_template.name,
    });
    optional_string(&mut value, "title", resource_template.title.as_ref());
    optional_string(
        &mut value,
        "description",
        resource_template.description.as_ref(),
    );
    optional_string(&mut value, "mimeType", resource_template.mime_type.as_ref());
    if !resource_template.icons.is_empty() {
        value["icons"] = icons_to_value(&resource_template.icons);
    }
    optional_value(
        &mut value,
        "annotations",
        resource_template.annotations.as_ref(),
    );
    optional_value(&mut value, "_meta", resource_template.meta.as_ref());
    value
}

fn optional_string(value: &mut Value, key: &str, item: Option<&String>) {
    if let Some(item) = item {
        value[key] = json!(item);
    }
}

fn optional_value(value: &mut Value, key: &str, item: Option<&Value>) {
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
