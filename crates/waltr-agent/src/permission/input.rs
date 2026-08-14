//! Strict JSON input permission matching.

use std::{cmp::Ordering, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    PermissionClaim, PermissionEffect, PermissionLevel, PermissionOperation, PermissionResource,
};

/// Canonical strict RFC 6901 JSON Pointer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PermissionJsonPointer(String);

impl PermissionJsonPointer {
    /// Parse and canonicalize a strict RFC 6901 pointer.
    ///
    /// # Errors
    ///
    /// Returns an error when a nonempty pointer does not start with `/` or contains an invalid
    /// RFC 6901 escape.
    pub fn parse(pointer: impl Into<String>) -> Result<Self, String> {
        let pointer = pointer.into();
        if pointer.is_empty() {
            return Ok(Self(pointer));
        }
        if !pointer.starts_with('/') {
            return Err("nonempty JSON Pointer must start with `/`".to_owned());
        }
        let mut canonical = String::new();
        for token in pointer[1..].split('/') {
            canonical.push('/');
            let decoded = decode_token(token)?;
            canonical.push_str(&decoded.replace('~', "~0").replace('/', "~1"));
        }
        Ok(Self(canonical))
    }

    /// Return the canonical pointer string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn select<'a>(&self, value: &'a Value) -> Option<&'a Value> {
        if self.0.is_empty() {
            return Some(value);
        }
        let mut selected = value;
        for token in self.0[1..].split('/') {
            let token = decode_token(token).expect("stored pointer is canonical");
            selected = match selected {
                Value::Object(object) => object.get(&token)?,
                Value::Array(array) => array.get(canonical_array_index(&token)?)?,
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => return None,
            };
        }
        Some(selected)
    }
}

impl TryFrom<String> for PermissionJsonPointer {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<PermissionJsonPointer> for String {
    fn from(value: PermissionJsonPointer) -> Self {
        value.0
    }
}

/// Exact typed scalar used by input policies and durable approvals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PermissionInputScalar {
    /// JSON null.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// Canonical mathematical decimal value.
    Number(String),
    /// JSON string.
    String(String),
}

impl PermissionInputScalar {
    /// Convert a JSON scalar into its exact typed representation.
    ///
    /// # Errors
    ///
    /// Returns an error for arrays, objects, or numbers that cannot be represented canonically.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        match value {
            Value::Null => Ok(Self::Null),
            Value::Bool(value) => Ok(Self::Bool(*value)),
            Value::Number(value) => canonical_number(&value.to_string()).map(Self::Number),
            Value::String(value) => Ok(Self::String(value.clone())),
            Value::Array(_) | Value::Object(_) => {
                Err("exact input value must be a JSON scalar".to_owned())
            }
        }
    }

    pub(crate) fn number(source: &str) -> Result<Self, String> {
        canonical_number(source).map(Self::Number)
    }
}

/// Value matcher in one input condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionInputValueMatcher {
    /// Exact typed scalar equality.
    Exact(PermissionInputScalar),
    /// String prefix; the policy source's terminal `*` is omitted.
    WildcardPrefix(String),
}

/// One pointer/value condition in an input rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionInputCondition {
    /// Canonical JSON Pointer.
    pub pointer: PermissionJsonPointer,
    /// Exact or terminal-wildcard value matcher.
    pub value: PermissionInputValueMatcher,
}

/// Ordered AND input rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionInputRule {
    /// Nonempty ordered conditions.
    pub conditions: Vec<PermissionInputCondition>,
    /// Rule effect.
    pub effect: PermissionEffect,
}

impl PermissionInputRule {
    /// Create a validated input rule.
    ///
    /// # Errors
    ///
    /// Returns an error when `conditions` is empty.
    pub fn new(
        conditions: Vec<PermissionInputCondition>,
        effect: PermissionEffect,
    ) -> Result<Self, String> {
        if conditions.is_empty() {
            return Err("input rule match must contain at least one condition".to_owned());
        }
        Ok(Self { conditions, effect })
    }
}

#[derive(Debug, Clone)]
pub struct PermissionInputEvaluation {
    pub(crate) effect: PermissionEffect,
    pub(crate) claims: Vec<PermissionClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledInputRule {
    pub(crate) selector: super::PermissionToolSelector,
    pub(crate) rule: PermissionInputRule,
    pub(crate) position: usize,
}

impl CompiledInputRule {
    pub(crate) fn evaluate(&self, tool_name: &str, arguments: &Value) -> InputRuleEvaluation {
        if self.selector.specificity(tool_name).is_none() {
            return InputRuleEvaluation::Missing;
        }
        let mut observed = Vec::with_capacity(self.rule.conditions.len());
        let mut all_match = true;
        for condition in &self.rule.conditions {
            let Some(selected) = condition.pointer.select(arguments) else {
                all_match = false;
                continue;
            };
            if selected.is_array() || selected.is_object() {
                return InputRuleEvaluation::Aggregate;
            }
            let scalar = PermissionInputScalar::from_json(selected)
                .expect("non-aggregate JSON values are scalar");
            let matches = match &condition.value {
                PermissionInputValueMatcher::Exact(expected) => expected == &scalar,
                PermissionInputValueMatcher::WildcardPrefix(prefix) => {
                    matches!(&scalar, PermissionInputScalar::String(value) if value.starts_with(prefix))
                }
            };
            if !matches {
                all_match = false;
            }
            observed.push((condition.pointer.clone(), scalar));
        }
        if !all_match {
            return InputRuleEvaluation::Missing;
        }
        InputRuleEvaluation::Match(InputMatch {
            condition_count: self.rule.conditions.len(),
            exact_count: self
                .rule
                .conditions
                .iter()
                .filter(|condition| {
                    matches!(condition.value, PermissionInputValueMatcher::Exact(_))
                })
                .count(),
            wildcard_prefix_length: self
                .rule
                .conditions
                .iter()
                .filter_map(|condition| match &condition.value {
                    PermissionInputValueMatcher::WildcardPrefix(prefix) => Some(prefix.len()),
                    PermissionInputValueMatcher::Exact(_) => None,
                })
                .sum(),
            effect: self.rule.effect.clone(),
            position: self.position,
            observed,
        })
    }
}

pub enum InputRuleEvaluation {
    Missing,
    Aggregate,
    Match(InputMatch),
}

#[derive(Debug, Clone, Eq)]
pub struct InputMatch {
    condition_count: usize,
    exact_count: usize,
    wildcard_prefix_length: usize,
    pub(crate) effect: PermissionEffect,
    position: usize,
    pub(crate) observed: Vec<(PermissionJsonPointer, PermissionInputScalar)>,
}

impl Ord for InputMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.condition_count
            .cmp(&other.condition_count)
            .then_with(|| self.exact_count.cmp(&other.exact_count))
            .then_with(|| {
                self.wildcard_prefix_length
                    .cmp(&other.wildcard_prefix_length)
            })
            .then_with(|| {
                self.effect
                    .level
                    .conflict_rank()
                    .cmp(&other.effect.level.conflict_rank())
            })
            .then_with(|| self.position.cmp(&other.position))
    }
}

impl PartialOrd for InputMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for InputMatch {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

pub fn input_claims(
    tool_name: &str,
    observed: Vec<(PermissionJsonPointer, PermissionInputScalar)>,
) -> Vec<PermissionClaim> {
    observed
        .into_iter()
        .map(|(pointer, value)| {
            PermissionClaim::new(
                PermissionOperation::Execute,
                PermissionResource::ToolInput {
                    tool: tool_name.to_owned(),
                    pointer,
                    value,
                },
                format!("input approval for tool `{tool_name}`"),
            )
        })
        .collect()
}

fn decode_token(token: &str) -> Result<String, String> {
    let mut decoded = String::new();
    let mut chars = token.chars();
    while let Some(character) = chars.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            Some(other) => return Err(format!("invalid JSON Pointer escape `~{other}`")),
            None => return Err("trailing `~` in JSON Pointer".to_owned()),
        }
    }
    Ok(decoded)
}

fn canonical_array_index(token: &str) -> Option<usize> {
    if token == "0" {
        return Some(0);
    }
    if token.is_empty()
        || token.starts_with('0')
        || !token.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    token.parse().ok()
}

fn canonical_number(number: &str) -> Result<String, String> {
    let (negative, unsigned) = number
        .strip_prefix('-')
        .map_or((false, number), |value| (true, value));
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, 0_i64), |(mantissa, exponent)| {
            (mantissa, exponent.parse::<i64>().unwrap_or(i64::MAX))
        });
    if exponent == i64::MAX {
        return Err("numeric exponent is out of range".to_owned());
    }
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = format!("{integer}{fraction}");
    let leading = digits.bytes().take_while(|byte| *byte == b'0').count();
    digits.drain(..leading);
    if digits.is_empty() {
        return Ok("0".to_owned());
    }
    let trailing = digits
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'0')
        .count();
    digits.truncate(digits.len() - trailing);
    let scale = exponent
        .checked_sub(i64::try_from(fraction.len()).map_err(|_| "number is too long")?)
        .and_then(|value| value.checked_add(i64::try_from(trailing).ok()?))
        .ok_or_else(|| "numeric exponent is out of range".to_owned())?;
    Ok(format!(
        "{}{digits}e{scale}",
        if negative { "-" } else { "" }
    ))
}

impl fmt::Display for PermissionJsonPointer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub const fn denied_input_evaluation() -> PermissionInputEvaluation {
    PermissionInputEvaluation {
        effect: PermissionEffect::deny(),
        claims: Vec::new(),
    }
}

pub fn matched_input_evaluation(tool_name: &str, matched: InputMatch) -> PermissionInputEvaluation {
    let claims = if matched.effect.level == PermissionLevel::Ask {
        input_claims(tool_name, matched.observed)
    } else {
        Vec::new()
    };
    PermissionInputEvaluation {
        effect: matched.effect,
        claims,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{PermissionPolicy, PermissionToolSelector};

    fn exact(pointer: &str, value: &Value) -> PermissionInputCondition {
        PermissionInputCondition {
            pointer: PermissionJsonPointer::parse(pointer).unwrap(),
            value: PermissionInputValueMatcher::Exact(
                PermissionInputScalar::from_json(value).unwrap(),
            ),
        }
    }

    fn wildcard(pointer: &str, prefix: &str) -> PermissionInputCondition {
        PermissionInputCondition {
            pointer: PermissionJsonPointer::parse(pointer).unwrap(),
            value: PermissionInputValueMatcher::WildcardPrefix(prefix.to_owned()),
        }
    }

    #[test]
    fn pointer_is_strict_and_array_indices_are_canonical() {
        assert_eq!(PermissionJsonPointer::parse("").unwrap().as_str(), "");
        assert_eq!(
            PermissionJsonPointer::parse("/a~1b/~0key")
                .unwrap()
                .as_str(),
            "/a~1b/~0key"
        );
        for invalid in ["root", "/bad~", "/bad~2"] {
            assert!(PermissionJsonPointer::parse(invalid).is_err(), "{invalid}");
        }
        let value = json!({ "array": ["zero", "one"], "01": "object key" });
        assert_eq!(
            PermissionJsonPointer::parse("/array/1")
                .unwrap()
                .select(&value),
            Some(&json!("one"))
        );
        assert!(
            PermissionJsonPointer::parse("/array/01")
                .unwrap()
                .select(&value)
                .is_none()
        );
        assert_eq!(
            PermissionJsonPointer::parse("/01").unwrap().select(&value),
            Some(&json!("object key"))
        );
    }

    #[test]
    fn exact_numbers_use_mathematical_decimal_equality() {
        let scalar = |source| {
            PermissionInputScalar::from_json(&serde_json::from_str(source).unwrap()).unwrap()
        };
        assert_eq!(scalar("1"), scalar("1.0"));
        assert_eq!(scalar("1.0"), scalar("1e0"));
        assert_ne!(scalar("1"), PermissionInputScalar::String("1".to_owned()));
        assert_eq!(
            scalar("0.0000000000000000001"),
            PermissionInputScalar::Number("1e-19".to_owned())
        );
    }

    #[test]
    fn precedence_uses_condition_count_before_other_dimensions() {
        let selector = PermissionToolSelector::exact("tool");
        let policy = PermissionPolicy::default()
            .with_input_rule(
                selector.clone(),
                PermissionInputRule::new(
                    vec![exact("/action", &json!("sessions.read"))],
                    PermissionEffect::deny(),
                )
                .unwrap(),
            )
            .with_input_rule(
                selector,
                PermissionInputRule::new(
                    vec![
                        wildcard("/action", "sessions."),
                        exact("/options/dryRun", &json!(false)),
                    ],
                    PermissionEffect::ask(),
                )
                .unwrap(),
            );
        let evaluated = policy
            .evaluate_input(
                "tool",
                &json!({ "action": "sessions.read", "options": { "dryRun": false } }),
            )
            .unwrap();
        assert_eq!(evaluated.effect.level, PermissionLevel::Ask);
        assert_eq!(evaluated.claims.len(), 2);
    }

    #[test]
    fn aggregate_selection_denies_and_missing_does_not_match() {
        let policy = PermissionPolicy::default().with_input_rule(
            PermissionToolSelector::exact("tool"),
            PermissionInputRule::new(vec![wildcard("/options", "")], PermissionEffect::allow())
                .unwrap(),
        );
        assert!(policy.evaluate_input("tool", &json!({})).is_none());
        assert_eq!(
            policy
                .evaluate_input("tool", &json!({ "options": {} }))
                .unwrap()
                .effect
                .level,
            PermissionLevel::Deny
        );
    }

    #[test]
    fn wildcard_ask_claim_is_exact_tool_bound_and_summary_hides_value() {
        let policy = PermissionPolicy::default().with_input_rule(
            PermissionToolSelector::exact("tool"),
            PermissionInputRule::new(
                vec![wildcard("/action", "sessions.")],
                PermissionEffect::ask(),
            )
            .unwrap(),
        );
        let evaluated = policy
            .evaluate_input("tool", &json!({ "action": "sessions.read" }))
            .unwrap();
        assert!(matches!(
            &evaluated.claims[0].resource,
            PermissionResource::ToolInput { tool, pointer, value }
                if tool == "tool"
                    && pointer.as_str() == "/action"
                    && value == &PermissionInputScalar::String("sessions.read".to_owned())
        ));
        assert_eq!(
            evaluated.claims[0].summary,
            "input approval for tool `tool`"
        );
    }
}
