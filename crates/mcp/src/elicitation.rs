//! Elicitation through the 2026-07-28 multi round-trip request mechanism.

use std::{collections::BTreeMap, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Error, Result};

/// Object-safe asynchronous client elicitation callback result.
pub type McpElicitationFuture =
    Pin<Box<dyn Future<Output = Result<McpElicitationResponse>> + Send + 'static>>;

/// Explicitly supported elicitation modes. Both are disabled by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpElicitationCapabilities {
    /// In-band, non-sensitive form data.
    pub form: bool,
    /// Out-of-band URL interaction; the host owns consent and navigation.
    pub url: bool,
}

impl McpElicitationCapabilities {
    /// Decode per-request capabilities, including the implicit form-only `{}` spelling.
    pub fn parse(capabilities: &Value) -> Result<Self> {
        let Some(value) = capabilities.get("elicitation") else {
            return Ok(Self::default());
        };
        let object = value
            .as_object()
            .ok_or_else(|| invalid("capability must be an object"))?;
        for mode in ["form", "url"] {
            if object.get(mode).is_some_and(|value| !value.is_object()) {
                return Err(invalid("mode capability must be an object"));
            }
        }
        let modes = Self {
            form: object.is_empty() || object.contains_key("form"),
            url: object.contains_key("url"),
        };
        if !modes.form && !modes.url {
            return Err(invalid("capability must support at least one mode"));
        }
        Ok(modes)
    }

    /// Return the capability value, or None when no mode is enabled.
    #[must_use]
    pub fn to_value(self) -> Option<Value> {
        let mut value = serde_json::Map::new();
        if self.form {
            value.insert("form".to_owned(), json!({}));
        }
        if self.url {
            value.insert("url".to_owned(), json!({}));
        }
        (!value.is_empty()).then_some(Value::Object(value))
    }

    /// Whether the selected request mode was advertised.
    #[must_use]
    pub const fn supports(self, request: &McpElicitationRequest) -> bool {
        match request {
            McpElicitationRequest::Form { .. } => self.form,
            McpElicitationRequest::Url { .. } => self.url,
        }
    }
}

/// Flat form schema. Property schemas retain titles, defaults, and enum labels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpElicitationSchema {
    /// Optional JSON Schema dialect URI.
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// Must be `object`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Primitive property schemas, including string-enum arrays.
    pub properties: BTreeMap<String, Value>,
    /// Required property names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

/// Parameters of an `elicitation/create` input request, not a JSON-RPC request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum McpElicitationRequest {
    /// Non-sensitive structured data presented for user review.
    Form {
        /// Server-supplied explanation, not trusted instructions.
        message: String,
        /// Restricted flat object schema.
        #[serde(rename = "requestedSchema")]
        requested_schema: McpElicitationSchema,
    },
    /// Consent to an external interaction. Never automatically fetch or open this URL.
    Url {
        /// Server-supplied explanation.
        message: String,
        /// Full URL to display before navigation; host policy decides whether it is safe.
        url: String,
    },
}

impl McpElicitationRequest {
    /// Parse parameters, accepting omitted mode as form mode.
    pub fn parse(params: &Value) -> Result<Self> {
        let mut params = params.clone();
        let object = params
            .as_object_mut()
            .ok_or_else(|| invalid("params must be an object"))?;
        object.entry("mode").or_insert_with(|| json!("form"));
        let request: Self = serde_json::from_value(params)
            .map_err(|error| invalid(&format!("invalid request: {error}")))?;
        request.validate()?;
        Ok(request)
    }

    /// Validate the protocol shape without fetching URLs or applying host trust policy.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Form {
                requested_schema, ..
            } => requested_schema.validate(),
            Self::Url { url, .. } => url::Url::parse(url)
                .map(|_| ())
                .map_err(|_| invalid("URL must be absolute and valid")),
        }
    }

    /// Construct a keyed MRTR input request value.
    pub fn to_input_request(&self) -> Result<Value> {
        self.validate()?;
        Ok(json!({ "method": "elicitation/create", "params": self }))
    }

    /// Validate response shape and submitted values. String format policy belongs to the host.
    pub fn validate_response(&self, response: &McpElicitationResponse) -> Result<()> {
        if response.action != McpElicitationAction::Accept {
            return if response.content.is_none() {
                Ok(())
            } else {
                Err(invalid("non-accept response contains content"))
            };
        }
        match self {
            Self::Url { .. } if response.content.is_some() => {
                Err(invalid("URL response contains content"))
            }
            Self::Url { .. } => Ok(()),
            Self::Form {
                requested_schema, ..
            } => {
                let content = response
                    .content
                    .as_ref()
                    .ok_or_else(|| invalid("accepted form omitted content"))?;
                requested_schema.validate_content(content)
            }
        }
    }
}

/// User intent, distinct from cancellation of the originating operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpElicitationAction {
    /// Submitted form data or consented to URL navigation (not URL completion).
    Accept,
    /// Explicit refusal.
    Decline,
    /// Dismissed the interaction without a choice.
    Cancel,
}

/// Allowed flat form values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpElicitationValue {
    /// Text or a single enum choice.
    String(String),
    /// JSON number, including integers.
    Number(serde_json::Number),
    /// Boolean choice.
    Boolean(bool),
    /// Multiple string enum choices.
    Strings(Vec<String>),
}

/// Keyed MRTR response. It deliberately has no JSON-RPC ID or resultType.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpElicitationResponse {
    /// Explicit user intent.
    pub action: McpElicitationAction,
    /// Submitted form data, only for accepted forms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<BTreeMap<String, McpElicitationValue>>,
}

impl McpElicitationSchema {
    /// Reject nested schemas and malformed primitive/enum definitions.
    pub fn validate(&self) -> Result<()> {
        if self.schema_type != "object"
            || self.required.iter().any(|key| !self.properties.contains_key(key))
        {
            return Err(invalid(
                "form schema must be a flat object with known required properties",
            ));
        }
        for property in self.properties.values() {
            validate_property(property)?;
        }
        Ok(())
    }

    fn validate_content(&self, content: &BTreeMap<String, McpElicitationValue>) -> Result<()> {
        self.validate()?;
        if self.required.iter().any(|key| !content.contains_key(key)) {
            return Err(invalid("required form value missing"));
        }
        for (key, value) in content {
            let schema = self
                .properties
                .get(key)
                .ok_or_else(|| invalid("unknown form property"))?;
            let value = serde_json::to_value(value)
                .map_err(|error| invalid(&error.to_string()))?;
            validate_value(schema, &value)?;
        }
        Ok(())
    }
}

fn validate_property(schema: &Value) -> Result<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| invalid("property schema must be an object"))?;
    let kind = schema
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("property type missing"))?;
    let specific: &[&str] = match kind {
        "string" => &["minLength", "maxLength", "format", "enum", "enumNames", "oneOf"],
        "number" | "integer" => &["minimum", "maximum"],
        "boolean" => &[],
        "array" => &["minItems", "maxItems", "items"],
        _ => return Err(invalid("nested or unsupported property type")),
    };
    for (key, value) in object {
        if !["type", "title", "description", "default"].contains(&key.as_str())
            && !specific.contains(&key.as_str())
        {
            return Err(invalid("unsupported property schema keyword"));
        }
        match key.as_str() {
            "title" | "description" if !value.is_string() => {
                return Err(invalid("schema label must be a string"));
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" if value.as_u64().is_none() => {
                return Err(invalid("schema bound must be a non-negative integer"));
            }
            "minimum" | "maximum" if !value.is_number() => {
                return Err(invalid("schema bound must be numeric"));
            }
            "format" if !matches!(value.as_str(), Some("email" | "uri" | "date" | "date-time")) => {
                return Err(invalid("unsupported string format"));
            }
            "enum" | "enumNames" => string_options(value)?,
            "oneOf" => titled_options(value)?,
            _ => {}
        }
    }
    for (lower, upper) in [
        ("minLength", "maxLength"),
        ("minItems", "maxItems"),
        ("minimum", "maximum"),
    ] {
        if let (Some(lower), Some(upper)) = (
            schema.get(lower).and_then(Value::as_f64),
            schema.get(upper).and_then(Value::as_f64),
        ) {
            if lower > upper {
                return Err(invalid("inverted schema bounds"));
            }
        }
    }
    if kind == "array" {
        let items = schema
            .get("items")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("multi-select items missing"))?;
        if items.len() == 2
            && items.get("type").and_then(Value::as_str) == Some("string")
            && items.contains_key("enum")
        {
            string_options(&items["enum"])?;
        } else if items.len() == 1 && items.contains_key("anyOf") {
            titled_options(&items["anyOf"])?;
        } else {
            return Err(invalid("arrays must contain string enum choices"));
        }
    }
    if let Some(default) = schema.get("default") {
        validate_value(schema, default)?;
    }
    Ok(())
}

fn string_options(value: &Value) -> Result<()> {
    if value
        .as_array()
        .is_some_and(|values| !values.is_empty() && values.iter().all(Value::is_string))
    {
        Ok(())
    } else {
        Err(invalid("enum options must be non-empty strings"))
    }
}

fn titled_options(value: &Value) -> Result<()> {
    if value.as_array().is_some_and(|values| {
        !values.is_empty()
            && values.iter().all(|value| {
                value.as_object().is_some_and(|value| {
                    value.len() == 2
                        && value.get("const").is_some_and(Value::is_string)
                        && value.get("title").is_some_and(Value::is_string)
                })
            })
    }) {
        Ok(())
    } else {
        Err(invalid("invalid titled enum options"))
    }
}

fn validate_value(schema: &Value, value: &Value) -> Result<()> {
    let valid_type = match schema.get("type").and_then(Value::as_str) {
        Some("string") => value.is_string(),
        Some("number") => value.is_number(),
        Some("integer") => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().is_some_and(|number| number.fract() == 0.0)
        }
        Some("boolean") => value.is_boolean(),
        Some("array") => value
            .as_array()
            .is_some_and(|values| values.iter().all(Value::is_string)),
        _ => false,
    };
    if !valid_type {
        return Err(invalid("form value does not match property type"));
    }
    if let Some(text) = value.as_str() {
        validate_size(schema, "minLength", "maxLength", text.chars().count())?;
        validate_choice(schema, value, "oneOf")?;
    }
    if let Some(number) = value.as_f64() {
        if schema.get("minimum").and_then(Value::as_f64).is_some_and(|min| number < min)
            || schema.get("maximum").and_then(Value::as_f64).is_some_and(|max| number > max)
        {
            return Err(invalid("form number outside bounds"));
        }
    }
    if let Some(values) = value.as_array() {
        validate_size(schema, "minItems", "maxItems", values.len())?;
        for value in values {
            validate_choice(&schema["items"], value, "anyOf")?;
        }
    }
    Ok(())
}

fn validate_size(schema: &Value, min: &str, max: &str, size: usize) -> Result<()> {
    let size = u64::try_from(size).map_err(|_| invalid("form value too large"))?;
    if schema.get(min).and_then(Value::as_u64).is_some_and(|min| size < min)
        || schema.get(max).and_then(Value::as_u64).is_some_and(|max| size > max)
    {
        return Err(invalid("form value length outside bounds"));
    }
    Ok(())
}

fn validate_choice(schema: &Value, value: &Value, titled: &str) -> Result<()> {
    if schema.get("enum").and_then(Value::as_array).is_some_and(|choices| !choices.contains(value))
        || schema.get(titled).and_then(Value::as_array).is_some_and(|choices| {
            !choices.iter().any(|choice| choice.get("const") == Some(value))
        })
    {
        return Err(invalid("form value is not an enum choice"));
    }
    Ok(())
}

fn invalid(message: &str) -> Error {
    Error::protocol(format!("MCP elicitation: {message}"))
}
