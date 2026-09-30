//! Runtime helpers for the opinionated progressive MCP server pattern.

use jaq_interpret::{Ctx, FilterT as _, ParseCtx, RcIter, Val};
use schemars::generate::SchemaSettings;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    McpResourceDefinition, McpResourceResult, McpResourceTemplateDefinition, McpToolDefinition,
    McpToolResult,
    server::{ServerError, ServerResult},
};

/// Parsed arguments for a progressive tool.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressiveRequest {
    /// Flat or dot-notated action name.
    pub action: String,
    /// Action-specific input.
    #[serde(default)]
    pub input: Option<Value>,
    /// Optional jq-compatible filter applied to the JSON output.
    #[serde(default)]
    pub filter: Option<String>,
}

/// Output accepted from a generated progressive action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressiveActionOutput {
    /// JSON output converted into text and structured content.
    Json(Value),
    /// Complete semantic MCP tool result.
    Tool(McpToolResult),
}

impl From<Value> for ProgressiveActionOutput {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

impl From<McpToolResult> for ProgressiveActionOutput {
    fn from(result: McpToolResult) -> Self {
        Self::Tool(result)
    }
}

/// JSON schema for one configured action input.
#[derive(Debug, Clone)]
pub struct ActionInputSchema {
    /// Action that accepts this input schema.
    pub action: &'static str,
    /// JSON schema for the input, or `None` for no-input actions.
    pub schema: Option<Value>,
}

/// Parse progressive tool arguments.
///
/// # Errors
///
/// Returns invalid parameters when the arguments are malformed or omit the action name.
pub fn parse_arguments(tool: &str, arguments: Value) -> ServerResult<ProgressiveRequest> {
    let request = serde_json::from_value::<ProgressiveRequest>(arguments).map_err(|error| {
        ServerError::invalid_params(format!("invalid `{tool}` arguments: {error}"))
    })?;
    if request.action.trim().is_empty() {
        return Err(ServerError::invalid_params(format!(
            "missing `{tool}` action"
        )));
    }
    Ok(request)
}

/// Deserialize action input.
///
/// # Errors
///
/// Returns invalid parameters when input is absent or does not deserialize as `T`.
pub fn deserialize_action_input<T>(action: &str, input: Option<Value>) -> ServerResult<T>
where
    T: serde::de::DeserializeOwned,
{
    let input = input.ok_or_else(|| {
        ServerError::invalid_params(format!("missing input for action `{action}`"))
    })?;
    serde_json::from_value(input).map_err(|error| {
        ServerError::invalid_params(format!("invalid input for action `{action}`: {error}"))
    })
}

/// Reject input supplied to a no-input action.
///
/// # Errors
///
/// Returns invalid parameters when non-empty input is supplied.
pub fn ensure_no_action_input(action: &str, input: Option<Value>) -> ServerResult<()> {
    match input {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(object)) if object.is_empty() => Ok(()),
        Some(_) => Err(ServerError::invalid_params(format!(
            "action `{action}` does not accept input"
        ))),
    }
}

/// Create a generated progressive tool definition.
#[must_use]
pub fn tool_definition(name: &str, description: &str, input_schema: Value) -> McpToolDefinition {
    McpToolDefinition {
        name: name.to_owned(),
        title: Some(name.to_owned()),
        description: Some(description.to_owned()),
        icons: Vec::new(),
        input_schema,
        output_schema: None,
        annotations: None,
        meta: None,
    }
}

/// Build a generated progressive tool input schema.
#[must_use]
pub fn input_schema(actions: &[&str], inputs: Vec<ActionInputSchema>) -> Value {
    let mut input_options = Vec::new();
    input_options.extend(inputs.into_iter().map(|input| {
        let mut schema = input.schema.unwrap_or_else(|| json!({ "type": "null" }));
        if let Value::Object(object) = &mut schema {
            object
                .entry("title".to_owned())
                .or_insert_with(|| Value::String(format!("{} input", input.action)));
            object
                .entry("description".to_owned())
                .or_insert_with(|| Value::String(format!("Use with action `{}`.", input.action)));
        }
        schema
    }));
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["action"],
        "properties": {
            "action": {
                "type": "string",
                "description": "Action to execute.",
                "enum": actions
            },
            "input": {
                "description": "Action-specific input.",
                "oneOf": input_options
            },
            "filter": {
                "type": ["string", "null"],
                "description": "Optional jq-compatible filter applied to the action JSON output. Examples: `.id`, `.[].id`, `[.[] | .id]`."
            }
        }
    })
}

/// Build a JSON schema for a Rust input type using `schemars`.
#[must_use]
pub fn json_schema_for<T>() -> Value
where
    T: schemars::JsonSchema,
{
    let settings = SchemaSettings::draft2020_12().with(|settings| {
        settings.inline_subschemas = true;
    });
    serde_json::to_value(settings.into_generator().into_root_schema_for::<T>())
        .unwrap_or_else(|_| json!({ "type": "object" }))
}

/// Convert an action JSON value into an MCP tool result, applying an optional jq filter.
///
/// # Errors
///
/// Returns invalid parameters when the jq filter cannot be compiled or evaluated.
pub fn tool_result(output: Value, filter: Option<&str>) -> ServerResult<McpToolResult> {
    let output = match filter.map(str::trim).filter(|filter| !filter.is_empty()) {
        Some(filter) => apply_filter(output, filter)?,
        None => output,
    };
    let text = text_for_value(&output);
    let structured_content = match output {
        Value::Object(_) => output,
        output => json!({ "result": output }),
    };
    Ok(McpToolResult::new(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured_content
    })))
}

/// Convert a typed progressive action output into an MCP tool result.
///
/// JSON output retains the behavior of [`tool_result`]. Semantic MCP output is validated and,
/// when successfully filtered, its structured and text content are synchronized.
///
/// # Errors
///
/// Returns invalid parameters for malformed semantic results or jq filter failures.
pub fn action_tool_result(
    output: impl Into<ProgressiveActionOutput>,
    filter: Option<&str>,
) -> ServerResult<McpToolResult> {
    match output.into() {
        ProgressiveActionOutput::Json(output) => tool_result(output, filter),
        ProgressiveActionOutput::Tool(mut result) => {
            validate_tool_result(&result.raw)?;
            if result.raw.get("isError").and_then(Value::as_bool) == Some(true) {
                return Ok(result);
            }
            if let Some(filter) = filter.map(str::trim).filter(|filter| !filter.is_empty()) {
                let structured = result
                    .raw
                    .get("structuredContent")
                    .cloned()
                    .ok_or_else(|| {
                        ServerError::invalid_params(
                            "cannot filter MCP tool result without `structuredContent`",
                        )
                    })?;
                let filtered = apply_filter(structured, filter)?;
                let text = serde_json::to_string(&filtered).map_err(|error| {
                    ServerError::invalid_params(format!(
                        "cannot serialize filtered MCP tool result: {error}"
                    ))
                })?;
                let content = result
                    .raw
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| {
                        ServerError::invalid_params("MCP tool result `content` must be an array")
                    })?;
                let mut synchronized = Vec::with_capacity(content.len().max(1));
                let mut kept_text = false;
                for mut block in std::mem::take(content) {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if kept_text {
                            continue;
                        }
                        block
                            .as_object_mut()
                            .ok_or_else(|| {
                                ServerError::invalid_params(
                                    "MCP tool result content block must be an object",
                                )
                            })?
                            .insert("text".to_owned(), Value::String(text.clone()));
                        kept_text = true;
                    }
                    synchronized.push(block);
                }
                if !kept_text {
                    synchronized.insert(0, json!({ "type": "text", "text": text }));
                }
                *content = synchronized;
                result.raw["structuredContent"] = filtered;
                validate_tool_result(&result.raw)?;
            }
            Ok(result)
        }
    }
}

/// Convert action JSON into an MCP resource result.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn json_resource_result(uri: String, value: Value) -> McpResourceResult {
    McpResourceResult::text(uri, "application/json", value.to_string())
}

/// Convert a typed progressive action output into an MCP JSON resource result.
///
/// # Errors
///
/// Returns invalid parameters when semantic output is malformed, represents an error, or omits
/// `structuredContent`.
pub fn action_resource_result(
    uri: String,
    output: impl Into<ProgressiveActionOutput>,
) -> ServerResult<McpResourceResult> {
    let value = match output.into() {
        ProgressiveActionOutput::Json(value) => value,
        ProgressiveActionOutput::Tool(result) => {
            validate_tool_result(&result.raw)?;
            if result.raw.get("isError").and_then(Value::as_bool) == Some(true) {
                return Err(ServerError::invalid_params(
                    "cannot expose an MCP semantic error as a resource",
                ));
            }
            result
                .raw
                .get("structuredContent")
                .cloned()
                .ok_or_else(|| {
                    ServerError::invalid_params(
                        "cannot expose MCP tool result without `structuredContent` as a resource",
                    )
                })?
        }
    };
    Ok(json_resource_result(uri, value))
}

/// Build a resource definition for an action.
#[must_use]
pub fn action_resource_definition(
    uri: &str,
    action: &str,
    description: &str,
) -> McpResourceDefinition {
    McpResourceDefinition {
        uri: uri.to_owned(),
        name: action.to_owned(),
        title: Some(action.to_owned()),
        description: Some(description.to_owned()),
        mime_type: Some("application/json".to_owned()),
        icons: Vec::new(),
        size: None,
        annotations: None,
        meta: None,
    }
}

/// Build a resource-template definition for an action.
#[must_use]
pub fn action_resource_template_definition(
    uri_template: &str,
    action: &str,
    description: &str,
) -> McpResourceTemplateDefinition {
    McpResourceTemplateDefinition {
        uri_template: uri_template.to_owned(),
        name: action.to_owned(),
        title: Some(action.to_owned()),
        description: Some(description.to_owned()),
        mime_type: Some("application/json".to_owned()),
        icons: Vec::new(),
        annotations: None,
        meta: None,
    }
}

/// Extract template variables from a URI.
#[must_use]
pub fn template_bindings(template: &str, uri: &str) -> Option<serde_json::Map<String, Value>> {
    let mut bindings = serde_json::Map::new();
    let mut remaining_template = template;
    let mut remaining_uri = uri;

    while let Some(open) = remaining_template.find('{') {
        let literal = &remaining_template[..open];
        if !remaining_uri.starts_with(literal) {
            return None;
        }
        remaining_uri = &remaining_uri[literal.len()..];

        let close = remaining_template[open + 1..].find('}')?;
        let name = &remaining_template[(open + 1)..=(open + close)];
        if name.is_empty() {
            return None;
        }
        let after_variable = &remaining_template[open + close + 2..];
        if after_variable.is_empty() {
            insert_binding(&mut bindings, name, remaining_uri)?;
            return Some(bindings);
        }

        let next_literal_end = after_variable.find('{').unwrap_or(after_variable.len());
        let next_literal = &after_variable[..next_literal_end];
        if next_literal.is_empty() {
            return None;
        }
        let next_literal_start = remaining_uri.find(next_literal)?;
        if next_literal_start == 0 {
            return None;
        }
        insert_binding(&mut bindings, name, &remaining_uri[..next_literal_start])?;
        remaining_uri = &remaining_uri[next_literal_start..];
        remaining_template = after_variable;
    }

    (remaining_uri == remaining_template).then_some(bindings)
}

/// Build an unknown-action error.
#[must_use]
pub fn unknown_action(action: &str) -> ServerError {
    ServerError::invalid_params(format!("unknown progressive action `{action}`"))
}

fn apply_filter(input: Value, filter: &str) -> ServerResult<Value> {
    const MAX_FILTER_OUTPUTS: usize = 256;

    let mut defs = ParseCtx::new(Vec::new());
    defs.insert_natives(jaq_core::core());
    defs.insert_defs(jaq_std::std());
    let (filter, parse_errors) = jaq_parse::parse(filter, jaq_parse::main());
    if !parse_errors.is_empty() {
        return Err(ServerError::invalid_params(format!(
            "invalid jq filter: {parse_errors:?}"
        )));
    }
    let filter = filter.ok_or_else(|| ServerError::invalid_params("empty jq filter"))?;
    let filter = defs.compile(filter);
    if !defs.errs.is_empty() {
        let errors = defs
            .errs
            .iter()
            .map(|(error, _)| error.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ServerError::invalid_params(format!(
            "invalid jq filter: {errors}"
        )));
    }
    let inputs = RcIter::new(std::iter::empty());
    let mut values = Vec::new();
    for value in filter.run((Ctx::new([], &inputs), Val::from(input))) {
        let value = value
            .map(Value::from)
            .map_err(|error| ServerError::invalid_params(format!("jq filter failed: {error}")))?;
        if values.len() == MAX_FILTER_OUTPUTS {
            return Err(ServerError::invalid_params(format!(
                "jq filter produced more than {MAX_FILTER_OUTPUTS} results"
            )));
        }
        values.push(value);
    }
    Ok(match values.as_slice() {
        [] => Value::Null,
        [value] => value.clone(),
        _ => Value::Array(values),
    })
}

fn validate_tool_result(raw: &Value) -> ServerResult<()> {
    let object = raw
        .as_object()
        .ok_or_else(|| ServerError::invalid_params("MCP tool result must be a JSON object"))?;
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ServerError::invalid_params("MCP tool result `content` must be an array"))?;
    for (index, entry) in content.iter().enumerate() {
        let entry = entry.as_object().ok_or_else(|| {
            ServerError::invalid_params(format!(
                "MCP tool result `content[{index}]` must be an object"
            ))
        })?;
        if !entry.get("type").is_some_and(Value::is_string) {
            return Err(ServerError::invalid_params(format!(
                "MCP tool result `content[{index}].type` must be a string"
            )));
        }
    }
    if object
        .get("isError")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(ServerError::invalid_params(
            "MCP tool result `isError` must be a boolean",
        ));
    }
    Ok(())
}

fn text_for_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn insert_binding(
    bindings: &mut serde_json::Map<String, Value>,
    name: &str,
    value: &str,
) -> Option<()> {
    if value.is_empty() {
        return None;
    }
    match bindings.insert(name.to_owned(), Value::String(value.to_owned())) {
        Some(Value::String(previous)) if previous != value => None,
        _ => Some(()),
    }
}
