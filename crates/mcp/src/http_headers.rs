use std::collections::HashSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use crate::{Error, Result};

const PREFIX: &str = "=?base64?";
const SUFFIX: &str = "?=";
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderDescriptor {
    pub name: String,
    pub path: Vec<String>,
    kind: HeaderValueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderValueKind {
    String,
    Integer,
    Boolean,
}

pub fn header_descriptors(schema: &Value) -> Result<Vec<HeaderDescriptor>> {
    if !schema.is_object() {
        return Err(Error::protocol("tool inputSchema must be an object"));
    }
    let mut descriptors = Vec::new();
    walk_schema(schema, &mut Vec::new(), false, &mut descriptors)?;
    let mut names = HashSet::new();
    for descriptor in &descriptors {
        if !names.insert(descriptor.name.to_ascii_lowercase()) {
            return Err(Error::protocol(format!(
                "duplicate x-mcp-header name `{}`",
                descriptor.name
            )));
        }
    }
    Ok(descriptors)
}

fn walk_schema(
    schema: &Value,
    path: &mut Vec<String>,
    reachable: bool,
    descriptors: &mut Vec<HeaderDescriptor>,
) -> Result<()> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if let Some(header) = object.get("x-mcp-header") {
        if !reachable || path.is_empty() {
            return Err(Error::protocol(
                "x-mcp-header must be statically reachable through properties",
            ));
        }
        let name = header
            .as_str()
            .filter(|name| valid_token(name))
            .ok_or_else(|| Error::protocol("x-mcp-header must be a non-empty HTTP token"))?;
        let kind = match object.get("type").and_then(Value::as_str) {
            Some("string") => HeaderValueKind::String,
            Some("integer") => HeaderValueKind::Integer,
            Some("boolean") => HeaderValueKind::Boolean,
            _ => {
                return Err(Error::protocol(
                    "x-mcp-header properties must have type string, integer, or boolean",
                ));
            }
        };
        descriptors.push(HeaderDescriptor {
            name: name.to_owned(),
            path: path.clone(),
            kind,
        });
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            path.push(name.clone());
            walk_schema(property, path, true, descriptors)?;
            path.pop();
        }
    }
    for (key, value) in object {
        if matches!(key.as_str(), "properties" | "x-mcp-header") {
            continue;
        }
        reject_unreachable_annotations(value)?;
    }
    Ok(())
}

fn reject_unreachable_annotations(value: &Value) -> Result<()> {
    match value {
        Value::Object(object) => {
            if object.contains_key("x-mcp-header") {
                return Err(Error::protocol(
                    "x-mcp-header is not statically reachable through properties",
                ));
            }
            for value in object.values() {
                reject_unreachable_annotations(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                reject_unreachable_annotations(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn parameter_headers(
    descriptors: &[HeaderDescriptor],
    arguments: Option<&Value>,
) -> Result<Vec<(String, String)>> {
    expected_parameter_values(descriptors, arguments).map(|values| {
        values
            .into_iter()
            .map(|(name, value)| (name, encode_header_value(&value)))
            .collect()
    })
}

pub fn expected_parameter_values(
    descriptors: &[HeaderDescriptor],
    arguments: Option<&Value>,
) -> Result<Vec<(String, String)>> {
    let mut values = Vec::new();
    for descriptor in descriptors {
        let Some(value) = value_at_path(arguments, &descriptor.path) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        values.push((
            format!("mcp-param-{}", descriptor.name),
            descriptor.string_value(value)?,
        ));
    }
    Ok(values)
}

impl HeaderDescriptor {
    fn string_value(&self, value: &Value) -> Result<String> {
        match self.kind {
            HeaderValueKind::String => value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| Error::protocol("x-mcp-header string argument has the wrong type")),
            HeaderValueKind::Boolean => value
                .as_bool()
                .map(|value| value.to_string())
                .ok_or_else(|| Error::protocol("x-mcp-header boolean argument has the wrong type")),
            HeaderValueKind::Integer => {
                let value = value.as_i64().ok_or_else(|| {
                    Error::protocol("x-mcp-header integer argument has the wrong type")
                })?;
                if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
                    return Err(Error::protocol(
                        "x-mcp-header integer argument exceeds the JavaScript safe range",
                    ));
                }
                Ok(value.to_string())
            }
        }
    }
}

fn value_at_path<'a>(mut value: Option<&'a Value>, path: &[String]) -> Option<&'a Value> {
    for segment in path {
        value = value?.as_object()?.get(segment);
    }
    value
}

pub fn encode_header_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let sentinel = value.starts_with(PREFIX) && value.ends_with(SUFFIX);
    let safe = !sentinel
        && !bytes.is_empty()
        && bytes
            .iter()
            .all(|byte| *byte == b'\t' || (0x20..=0x7e).contains(byte))
        && !bytes.first().is_some_and(u8::is_ascii_whitespace)
        && !bytes.last().is_some_and(u8::is_ascii_whitespace);
    if safe {
        value.to_owned()
    } else {
        format!("{PREFIX}{}{SUFFIX}", STANDARD.encode(bytes))
    }
}

pub fn decode_header_value(value: &str) -> Result<String> {
    if let Some(encoded) = value
        .strip_prefix(PREFIX)
        .and_then(|value| value.strip_suffix(SUFFIX))
    {
        let decoded = STANDARD
            .decode(encoded)
            .map_err(|_| Error::protocol("invalid MCP Base64 header sentinel"))?;
        return String::from_utf8(decoded)
            .map_err(|_| Error::protocol("MCP Base64 header is not UTF-8"));
    }
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes
            .iter()
            .any(|byte| *byte != b'\t' && !(0x20..=0x7e).contains(byte))
        || bytes.first().is_some_and(u8::is_ascii_whitespace)
        || bytes.last().is_some_and(u8::is_ascii_whitespace)
    {
        return Err(Error::protocol("invalid plain MCP header value"));
    }
    Ok(value.to_owned())
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn encodes_and_decodes_unsafe_and_sentinel_values() {
        for value in ["plain", " snow ", "cafe\u{301}", "=?base64?cGxhaW4=?="] {
            let encoded = encode_header_value(value);
            assert_eq!(decode_header_value(&encoded).unwrap(), value);
            if value != "plain" {
                assert!(encoded.starts_with(PREFIX));
            }
        }
    }

    #[test]
    fn validates_nested_header_descriptors_and_extracts_values() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tenant": { "type": "string", "x-mcp-header": "Tenant" },
                "routing": {
                    "type": "object",
                    "properties": {
                        "shard": { "type": "integer", "x-mcp-header": "Shard" },
                        "urgent": { "type": "boolean", "x-mcp-header": "Urgent" }
                    }
                }
            }
        });
        let descriptors = header_descriptors(&schema).unwrap();
        assert_eq!(descriptors.len(), 3);
        let mut values = expected_parameter_values(
            &descriptors,
            Some(&json!({ "tenant": "acme", "routing": { "shard": 7, "urgent": true } })),
        )
        .unwrap();
        values.sort();
        assert_eq!(
            values,
            vec![
                ("mcp-param-Shard".to_owned(), "7".to_owned()),
                ("mcp-param-Tenant".to_owned(), "acme".to_owned()),
                ("mcp-param-Urgent".to_owned(), "true".to_owned()),
            ]
        );
    }

    #[test]
    fn rejects_unreachable_duplicate_or_unsafe_descriptors() {
        for schema in [
            json!({ "type": "array", "items": { "type": "string", "x-mcp-header": "Item" } }),
            json!({ "type": "object", "properties": {
                "a": { "type": "string", "x-mcp-header": "Route" },
                "b": { "type": "string", "x-mcp-header": "route" }
            }}),
            json!({ "type": "object", "properties": {
                "value": { "type": "number", "x-mcp-header": "Route" }
            }}),
            json!({ "type": "object", "properties": {
                "value": { "type": "string", "x-mcp-header": "bad header" }
            }}),
        ] {
            assert!(header_descriptors(&schema).is_err());
        }
    }
}
