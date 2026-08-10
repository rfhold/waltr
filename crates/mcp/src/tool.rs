use std::fmt;

use serde_json::Value;

use crate::types::{McpIcon, parse_icons};

/// Parsed content from an MCP tool result.
#[derive(Clone, PartialEq, Eq)]
pub struct McpToolResultContent<'a> {
    /// Ordered content blocks returned by the tool.
    pub content: Vec<McpToolContent<'a>>,
    /// Optional structured result content.
    pub structured_content: Option<&'a Value>,
    /// Whether the tool reported an execution error.
    pub is_error: bool,
}

/// One recognized or extension MCP tool-result content block.
#[derive(Clone, PartialEq, Eq)]
pub enum McpToolContent<'a> {
    /// Plain text content.
    Text {
        /// Text returned by the tool.
        text: &'a str,
    },
    /// Base64-encoded image content.
    Image {
        /// Standard Base64 image data.
        data: &'a str,
        /// Declared image MIME type.
        mime_type: &'a str,
    },
    /// A content extension not interpreted by this crate.
    Unsupported {
        /// Content type discriminator.
        kind: &'a str,
        /// Complete extension block.
        raw: &'a Value,
    },
}

impl fmt::Debug for McpToolResultContent<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpToolResultContent")
            .field("content", &self.content)
            .field("has_structured_content", &self.structured_content.is_some())
            .field("is_error", &self.is_error)
            .finish()
    }
}

impl fmt::Debug for McpToolContent<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { text } => formatter
                .debug_struct("Text")
                .field("text_bytes", &text.len())
                .finish(),
            Self::Image { data, mime_type } => formatter
                .debug_struct("Image")
                .field("mime_type", mime_type)
                .field("encoded_bytes", &data.len())
                .finish_non_exhaustive(),
            Self::Unsupported { kind, .. } => formatter
                .debug_struct("Unsupported")
                .field("kind", kind)
                .finish_non_exhaustive(),
        }
    }
}

/// Malformed MCP tool-result content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpToolResultContentError {
    /// The result payload is not an object.
    ResultNotObject,
    /// The required content field is absent or is not an array.
    ContentNotArray,
    /// A content entry is not an object.
    BlockNotObject {
        /// Zero-based content-block index.
        index: usize,
    },
    /// A content entry has no string type discriminator.
    BlockTypeNotString {
        /// Zero-based content-block index.
        index: usize,
    },
    /// A text entry has no string text field.
    TextNotString {
        /// Zero-based content-block index.
        index: usize,
    },
    /// An image entry has no string data field.
    ImageDataNotString {
        /// Zero-based content-block index.
        index: usize,
    },
    /// An image entry has no string MIME type field.
    ImageMimeTypeNotString {
        /// Zero-based content-block index.
        index: usize,
    },
    /// The optional error marker is not a boolean.
    IsErrorNotBoolean,
}

impl std::fmt::Display for McpToolResultContentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResultNotObject => formatter.write_str("MCP tool result must be an object"),
            Self::ContentNotArray => {
                formatter.write_str("MCP tool result content must be an array")
            }
            Self::BlockNotObject { index } => {
                write!(
                    formatter,
                    "MCP tool result content block {index} must be an object"
                )
            }
            Self::BlockTypeNotString { index } => write!(
                formatter,
                "MCP tool result content block {index} type must be a string"
            ),
            Self::TextNotString { index } => write!(
                formatter,
                "MCP tool result text block {index} text must be a string"
            ),
            Self::ImageDataNotString { index } => write!(
                formatter,
                "MCP tool result image block {index} data must be a string"
            ),
            Self::ImageMimeTypeNotString { index } => write!(
                formatter,
                "MCP tool result image block {index} mimeType must be a string"
            ),
            Self::IsErrorNotBoolean => {
                formatter.write_str("MCP tool result isError must be a boolean")
            }
        }
    }
}

impl std::error::Error for McpToolResultContentError {}

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
    /// Optional MCP annotations.
    pub annotations: Option<Value>,
    /// Optional extension metadata.
    pub meta: Option<Value>,
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
    let input_schema = value.get("inputSchema")?.clone();
    let icons = parse_icons(value);
    if value
        .get("icons")
        .and_then(Value::as_array)
        .is_some_and(|raw| raw.len() != icons.len())
    {
        return None;
    }
    Some(McpToolDefinition {
        name,
        title,
        description,
        icons,
        input_schema,
        output_schema: value.get("outputSchema").cloned(),
        annotations: value.get("annotations").cloned(),
        meta: value.get("_meta").cloned(),
    })
}

/// Parse ordered content from an MCP tool result.
///
/// Unknown content types remain available as raw extension blocks. Known content types are
/// rejected when their required fields are malformed.
///
/// # Errors
///
/// Returns a typed error when the result or a known content block has an invalid shape.
pub fn parse_tool_result_content(
    result: &Value,
) -> Result<McpToolResultContent<'_>, McpToolResultContentError> {
    let result = result
        .as_object()
        .ok_or(McpToolResultContentError::ResultNotObject)?;
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or(McpToolResultContentError::ContentNotArray)?;
    let is_error = match result.get("isError") {
        Some(value) => value
            .as_bool()
            .ok_or(McpToolResultContentError::IsErrorNotBoolean)?,
        None => false,
    };
    let content = content
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let block = block
                .as_object()
                .ok_or(McpToolResultContentError::BlockNotObject { index })?;
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .ok_or(McpToolResultContentError::BlockTypeNotString { index })?;
            match kind {
                "text" => Ok(McpToolContent::Text {
                    text: block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or(McpToolResultContentError::TextNotString { index })?,
                }),
                "image" => Ok(McpToolContent::Image {
                    data: block
                        .get("data")
                        .and_then(Value::as_str)
                        .ok_or(McpToolResultContentError::ImageDataNotString { index })?,
                    mime_type: block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .ok_or(McpToolResultContentError::ImageMimeTypeNotString { index })?,
                }),
                _ => Ok(McpToolContent::Unsupported {
                    kind,
                    raw: &content[index],
                }),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(McpToolResultContent {
        content,
        structured_content: result.get("structuredContent"),
        is_error,
    })
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
