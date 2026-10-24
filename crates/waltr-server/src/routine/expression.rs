use std::collections::BTreeMap;

use serde_json::Value;

use crate::resource_catalog::routine::{RoutineCondition, RoutineConditionOperator, RoutineScalar};

use super::{InvocationError, engine::RoutineInvocationContext};

/// Maximum UTF-8 bytes in any fully expanded scalar string.
pub(super) const MAX_EXPANDED_STRING_BYTES: usize = 512 * 1024;
/// Maximum UTF-8 bytes in a fully expanded system or user prompt.
pub(super) const MAX_EXPANDED_PROMPT_BYTES: usize = 512 * 1024;
/// Maximum UTF-8 bytes in a metadata key presented to durable storage.
const MAX_METADATA_KEY_BYTES: usize = 16 * 1024;
/// Maximum JSON-encoded bytes in a metadata value presented to durable storage.
pub(super) const MAX_SERIALIZED_METADATA_VALUE_BYTES: usize = 16 * 1024;

pub(super) fn conditions_match(
    conditions: &[RoutineCondition],
    context: &RoutineInvocationContext,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
) -> bool {
    conditions
        .iter()
        .all(|condition| condition_matches(condition, context, task_results))
}

fn condition_matches(
    condition: &RoutineCondition,
    context: &RoutineInvocationContext,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
) -> bool {
    if let RoutineCondition::Any(condition) = condition {
        return condition
            .any
            .iter()
            .any(|condition| condition_matches(condition, context, task_results));
    }
    let (input, operator, values) = match condition {
        RoutineCondition::Input(condition) => {
            let input = match substitute_scalar(&condition.input, context, task_results, None, None)
            {
                Ok(value) if scalar(&value) => value,
                Ok(_) | Err(_) => {
                    tracing::debug!(
                        error_class = "condition_input",
                        "routine condition did not resolve to a scalar"
                    );
                    return false;
                }
            };
            (input, condition.operator, &condition.values)
        }
        RoutineCondition::Metadata(condition) => {
            let Some(metadata) = context
                .event
                .as_ref()
                .and_then(|event| event.data.get("session"))
                .and_then(|session| session.get("metadata"))
                .and_then(Value::as_object)
            else {
                return false;
            };
            (
                metadata
                    .get(&condition.metadata.key)
                    .cloned()
                    .unwrap_or(Value::Null),
                condition.metadata.operator,
                &condition.metadata.values,
            )
        }
        RoutineCondition::Any(_) => unreachable!("handled above"),
    };
    if operator == RoutineConditionOperator::GreaterThanOrEqual {
        let Some(input) = numeric_value(&input) else {
            return false;
        };
        return values.first().is_some_and(|candidate| {
            substitute_scalar(candidate, context, task_results, None, None)
                .ok()
                .and_then(|candidate| numeric_value(&candidate))
                .is_some_and(|candidate| input >= candidate)
        });
    }
    let found = values.iter().any(|candidate| {
        substitute_scalar(candidate, context, task_results, None, None)
            .ok()
            .is_some_and(|candidate| scalar_equal(&input, &candidate))
    });
    match operator {
        RoutineConditionOperator::In => found,
        RoutineConditionOperator::NotIn => !found,
        RoutineConditionOperator::GreaterThanOrEqual => unreachable!("handled above"),
    }
}

fn numeric_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(value) => value.as_u64(),
        Value::String(value)
            if value == "0"
                || (!value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit())) =>
        {
            value.parse().ok()
        }
        _ => None,
    }
}

fn scalar_equal(left: &Value, right: &Value) -> bool {
    left == right
        || scalar_format(left).is_some_and(|left| scalar_format(right).as_deref() == Some(&left))
}

const fn scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn scalar_format(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_owned()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) => Some(value.clone()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

pub(super) fn substitute_scalar(
    scalar: &RoutineScalar,
    context: &RoutineInvocationContext,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
    output: Option<&str>,
    action: Option<&Value>,
) -> Result<Value, InvocationError> {
    let value = match scalar {
        RoutineScalar::Boolean(value) => Ok(Value::Bool(*value)),
        RoutineScalar::Integer(value) => Ok(Value::Number((*value).into())),
        RoutineScalar::String(value) => whole_expression(value).map_or_else(
            || {
                if value.contains("$(") {
                    substitute_string(value, context, task_results, output, action)
                        .map(Value::String)
                } else {
                    Ok(Value::String(value.clone()))
                }
            },
            |expression| resolve_reference(expression, context, task_results, output, action),
        ),
    }?;
    ensure_expanded_value_bound(&value)?;
    Ok(value)
}

fn ensure_expanded_value_bound(value: &Value) -> Result<(), InvocationError> {
    if let Value::String(value) = value {
        return ensure_string_bound(
            value,
            MAX_EXPANDED_STRING_BYTES,
            "expanded_string",
            "expanded scalar string",
        );
    }
    let encoded = serde_json::to_vec(value)
        .map_err(|error| InvocationError::new("expanded_value_encode", error.to_string()))?;
    if encoded.len() > MAX_EXPANDED_STRING_BYTES {
        return Err(InvocationError::new(
            "expanded_value",
            format!("expanded scalar value exceeds {MAX_EXPANDED_STRING_BYTES} bytes"),
        ));
    }
    Ok(())
}

pub(super) fn substitute_string(
    input: &str,
    context: &RoutineInvocationContext,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
    output: Option<&str>,
    action: Option<&Value>,
) -> Result<String, InvocationError> {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("$(") {
        result.push_str(&rest[..start]);
        let expression = &rest[start + 2..];
        let end = expression.find(')').ok_or_else(|| {
            InvocationError::new("substitution", "unterminated substitution expression")
        })?;
        let value = resolve_reference(&expression[..end], context, task_results, output, action)?;
        ensure_expanded_value_bound(&value)?;
        let formatted = scalar_format(&value).ok_or_else(|| {
            InvocationError::new("substitution", "embedded substitution is not scalar")
        })?;
        if result.len().saturating_add(formatted.len()) > MAX_EXPANDED_STRING_BYTES {
            return Err(InvocationError::new(
                "expanded_string",
                format!("expanded string exceeds {MAX_EXPANDED_STRING_BYTES} bytes"),
            ));
        }
        result.push_str(&formatted);
        rest = &expression[end + 1..];
    }
    result.push_str(rest);
    ensure_string_bound(
        &result,
        MAX_EXPANDED_STRING_BYTES,
        "expanded_string",
        "expanded string",
    )?;
    Ok(result)
}

pub(super) fn ensure_string_bound(
    value: &str,
    maximum: usize,
    class: &'static str,
    description: &str,
) -> Result<(), InvocationError> {
    if value.len() > maximum {
        return Err(InvocationError::new(
            class,
            format!("{description} exceeds {maximum} bytes"),
        ));
    }
    Ok(())
}

pub(super) fn validate_metadata_mutation(key: &str, value: &Value) -> Result<(), InvocationError> {
    ensure_string_bound(
        key,
        MAX_METADATA_KEY_BYTES,
        "metadata_key_size",
        "metadata key",
    )?;
    if !scalar(value) {
        return Err(InvocationError::new(
            "metadata_value_type",
            "metadata value must resolve to a scalar",
        ));
    }
    let encoded = serde_json::to_vec(value)
        .map_err(|error| InvocationError::new("metadata_value_encode", error.to_string()))?;
    if encoded.len() > MAX_SERIALIZED_METADATA_VALUE_BYTES {
        return Err(InvocationError::new(
            "metadata_value_size",
            format!(
                "serialized metadata value exceeds {MAX_SERIALIZED_METADATA_VALUE_BYTES} bytes"
            ),
        ));
    }
    Ok(())
}

fn whole_expression(value: &str) -> Option<&str> {
    value
        .strip_prefix("$(")?
        .strip_suffix(')')
        .filter(|inner| !inner.contains("$("))
}

fn resolve_reference(
    expression: &str,
    context: &RoutineInvocationContext,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
    output: Option<&str>,
    action: Option<&Value>,
) -> Result<Value, InvocationError> {
    if let Some(path) = expression.strip_prefix("event.data.") {
        let event = context
            .event
            .as_ref()
            .ok_or_else(|| InvocationError::new("substitution", "event context is unavailable"))?;
        return path
            .split('.')
            .try_fold(&event.data, |value, segment| {
                value.get(segment).ok_or_else(|| {
                    InvocationError::new(
                        "substitution",
                        format!("missing event field `{expression}`"),
                    )
                })
            })
            .cloned();
    }
    if expression == "trigger.session" {
        return context
            .trigger
            .get("session")
            .cloned()
            .ok_or_else(|| InvocationError::new("substitution", "trigger session is unavailable"));
    }
    if let Some(path) = expression.strip_prefix("trigger.session.") {
        return resolve_path(&context.trigger["session"], path, expression);
    }
    if let Some(path) = expression.strip_prefix("trigger.context.") {
        return resolve_path(&context.trigger["context"], path, expression);
    }
    if let Some(name) = expression.strip_prefix("params.") {
        return context.params.get(name).cloned().ok_or_else(|| {
            InvocationError::new("substitution", format!("missing parameter `{name}`"))
        });
    }
    if expression == "sourceSession.session" {
        return context
            .source_values
            .get("session")
            .cloned()
            .ok_or_else(|| InvocationError::new("substitution", "source session is unavailable"));
    }
    if let Some(path) = expression.strip_prefix("sourceSession.") {
        return resolve_path(&context.source_values, path, expression);
    }
    if expression == "output.text" {
        return output
            .map(|value| Value::String(value.to_owned()))
            .ok_or_else(|| InvocationError::new("substitution", "output.text is unavailable"));
    }
    if let Some(field) = expression.strip_prefix("output.json.") {
        if field.is_empty()
            || !field
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(InvocationError::new(
                "substitution",
                "unsupported output JSON field",
            ));
        }
        let output = output
            .ok_or_else(|| InvocationError::new("substitution", "output JSON is unavailable"))?;
        ensure_string_bound(
            output,
            MAX_EXPANDED_STRING_BYTES,
            "output_json_size",
            "output JSON",
        )?;
        let value: Value = serde_json::from_str(output)
            .map_err(|_| InvocationError::new("output_json", "output must be a JSON object"))?;
        let object = value
            .as_object()
            .ok_or_else(|| InvocationError::new("output_json", "output must be a JSON object"))?;
        let selected = object.get(field).ok_or_else(|| {
            InvocationError::new("output_json_field", "output JSON field is missing")
        })?;
        if !matches!(selected, Value::String(_) | Value::Bool(_)) {
            return Err(InvocationError::new(
                "output_json_type",
                "output JSON field must be a string or boolean",
            ));
        }
        ensure_expanded_value_bound(selected)?;
        return Ok(selected.clone());
    }
    if let Some(path) = expression.strip_prefix("action.") {
        return resolve_path(
            action.ok_or_else(|| {
                InvocationError::new("substitution", "action result is unavailable")
            })?,
            path,
            expression,
        );
    }
    if let Some(reference) = expression.strip_prefix("tasks.") {
        let parts = reference.split('.').collect::<Vec<_>>();
        if parts.len() == 3 && parts[1] == "results" {
            return task_results
                .get(parts[0])
                .and_then(|results| results.get(parts[2]))
                .cloned()
                .ok_or_else(|| {
                    InvocationError::new(
                        "substitution",
                        format!("missing task result `{expression}`"),
                    )
                });
        }
    }
    Err(InvocationError::new(
        "substitution",
        format!("unsupported substitution `{expression}`"),
    ))
}

fn resolve_path(root: &Value, path: &str, expression: &str) -> Result<Value, InvocationError> {
    path.split('.')
        .try_fold(root, |value, segment| {
            value.get(segment).ok_or_else(|| {
                InvocationError::new("substitution", format!("missing value `{expression}`"))
            })
        })
        .cloned()
}

pub(super) fn scalar_string(value: &Value, description: &str) -> Result<String, InvocationError> {
    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
        InvocationError::new(
            "substitution_type",
            format!("{description} must be a string"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_catalog::routine::{RoutineAnyCondition, RoutineInputCondition};
    use serde_json::json;

    fn context(percent: Value) -> RoutineInvocationContext {
        RoutineInvocationContext {
            event: None,
            trigger: json!({"context": {"percent": percent}}),
            params: BTreeMap::new(),
            source: None,
            source_values: Value::Null,
        }
    }

    fn threshold(value: RoutineScalar) -> RoutineCondition {
        RoutineCondition::Input(RoutineInputCondition {
            input: RoutineScalar::String("$(trigger.context.percent)".to_owned()),
            operator: RoutineConditionOperator::GreaterThanOrEqual,
            values: vec![value],
        })
    }

    fn extract(output: Option<&str>, field: &str) -> Result<Value, InvocationError> {
        substitute_scalar(
            &RoutineScalar::String(format!("$(output.json.{field})")),
            &context(Value::Null),
            &BTreeMap::new(),
            output,
            None,
        )
    }

    #[test]
    fn output_json_extracts_only_bounded_top_level_strings_and_booleans() {
        assert_eq!(
            extract(Some(r#"{"text":"Speaker 1: hello","ok":true}"#), "text").unwrap(),
            json!("Speaker 1: hello")
        );
        assert_eq!(
            extract(Some(r#"{"ok":false}"#), "ok").unwrap(),
            json!(false)
        );
        for output in [
            "not JSON",
            "```json\n{}\n```",
            "{} trailing",
            "[]",
            "null",
            "true",
            "{}",
            r#"{"text":{}}"#,
            r#"{"text":[]}"#,
            r#"{"text":null}"#,
            r#"{"text":1}"#,
            r#"{"secret-payload":"unterminated}"#,
        ] {
            let error = extract(Some(output), "text").unwrap_err();
            assert!(!format!("{error:?}").contains("secret-payload"));
        }
        assert!(extract(None, "text").is_err());
        for field in ["", "text.nested", "text[0]", "*"] {
            assert!(extract(Some(r#"{"text":"ok"}"#), field).is_err());
        }
        let boundary =
            serde_json::to_string(&json!({"text": "x".repeat(MAX_EXPANDED_STRING_BYTES - 11)}))
                .unwrap();
        assert_eq!(boundary.len(), MAX_EXPANDED_STRING_BYTES);
        assert!(extract(Some(&boundary), "text").is_ok());
        let error = extract(Some(&(boundary + " ")), "text").unwrap_err();
        assert_eq!(error.class, "output_json_size");
        assert!(!format!("{error:?}").contains("xxxxx"));
        assert!(ensure_expanded_value_bound(&json!("x".repeat(MAX_EXPANDED_STRING_BYTES))).is_ok());
        assert!(
            ensure_expanded_value_bound(&json!("x".repeat(MAX_EXPANDED_STRING_BYTES + 1))).is_err()
        );
    }

    #[test]
    fn expanded_text_and_prompt_budgets_do_not_raise_metadata_limits() {
        assert_eq!(MAX_EXPANDED_STRING_BYTES, 512 * 1024);
        assert_eq!(MAX_EXPANDED_PROMPT_BYTES, 512 * 1024);
        assert_eq!(MAX_METADATA_KEY_BYTES, 16 * 1024);
        assert_eq!(MAX_SERIALIZED_METADATA_VALUE_BYTES, 16 * 1024);
        let source = "secret-payload".repeat(MAX_EXPANDED_STRING_BYTES / 14 + 1);
        let error = ensure_expanded_value_bound(&json!(source)).unwrap_err();
        assert_eq!(error.class, "expanded_string");
        assert!(!format!("{error:?}").contains("secret-payload"));
        let boundary = "x".repeat(MAX_EXPANDED_PROMPT_BYTES);
        assert!(
            substitute_string(
                &boundary,
                &context(Value::Null),
                &BTreeMap::new(),
                None,
                None
            )
            .is_ok()
        );
        let error = substitute_string(
            &(boundary + "x"),
            &context(Value::Null),
            &BTreeMap::new(),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.class, "expanded_string");
        assert!(
            validate_metadata_mutation(&"k".repeat(MAX_METADATA_KEY_BYTES), &json!(true)).is_ok()
        );
        let error =
            validate_metadata_mutation(&"k".repeat(MAX_METADATA_KEY_BYTES + 1), &json!(true))
                .unwrap_err();
        assert_eq!(error.class, "metadata_key_size");
        assert!(
            validate_metadata_mutation(
                "key",
                &json!("x".repeat(MAX_SERIALIZED_METADATA_VALUE_BYTES - 2))
            )
            .is_ok()
        );
        let error =
            validate_metadata_mutation("key", &json!("secret-payload".repeat(2000))).unwrap_err();
        assert_eq!(error.class, "metadata_value_size");
        assert!(!format!("{error:?}").contains("secret-payload"));
    }

    #[test]
    fn nested_any_and_numeric_comparison_are_strict() {
        let condition = RoutineCondition::Any(RoutineAnyCondition {
            any: vec![
                threshold(RoutineScalar::String("80".to_owned())),
                threshold(RoutineScalar::Integer(90)),
            ],
        });
        let results = BTreeMap::new();
        assert!(conditions_match(
            &[condition.clone()],
            &context(json!(80)),
            &results
        ));
        assert!(conditions_match(
            &[condition.clone()],
            &context(json!("90")),
            &results
        ));
        assert!(!conditions_match(
            &[condition.clone()],
            &context(Value::Null),
            &results
        ));
        assert!(!conditions_match(
            &[condition.clone()],
            &context(json!("080")),
            &results
        ));
        assert!(!conditions_match(
            &[condition],
            &context(json!("18446744073709551616")),
            &results
        ));
    }
}
