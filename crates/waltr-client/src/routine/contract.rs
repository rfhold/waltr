//! Public v2 values. Integer identities cross JavaScript as decimal strings.

use crate::ClientError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ts_rs::TS;

#[derive(Clone, Debug, Serialize, PartialEq, Eq, TS)]
#[serde(transparent)]
pub struct RoutineJson(String);
impl<'de> Deserialize<'de> for RoutineJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?)
            .map_err(|_| serde::de::Error::custom("invalid Routine JSON"))
    }
}

impl RoutineJson {
    pub fn parse(json: impl Into<String>) -> Result<Self, ClientError> {
        let json = json.into();
        if json.len() > 65536 {
            return Err(invalid());
        }
        let mut decoder = serde_json::Deserializer::from_str(&json);
        let value = ExactValue::deserialize(&mut decoder).map_err(|_| invalid())?;
        decoder.end().map_err(|_| invalid())?;
        value.validate(0)?;
        Ok(Self(json))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn value(&self) -> Result<serde_json::Value, ClientError> {
        serde_json::from_str(&self.0).map_err(|_| invalid())
    }
}

// A visitor rejects duplicate keys before serde_json::Value could erase them.
struct ExactValue(serde_json::Value);
impl ExactValue {
    fn validate(&self, depth: usize) -> Result<(), ClientError> {
        if depth > 16 {
            return Err(invalid());
        }
        match &self.0 {
            serde_json::Value::Number(n) => {
                let f = n.as_f64().ok_or_else(invalid)?;
                if !f.is_finite() || (f.fract() == 0.0 && f.abs() > 9_007_199_254_740_991.0) {
                    return Err(invalid());
                }
                if let Some(i) = n.as_i64()
                    && i as f64 != f
                {
                    return Err(invalid());
                }
                if let Some(i) = n.as_u64()
                    && i as f64 != f
                {
                    return Err(invalid());
                }
            }
            serde_json::Value::Array(a) => {
                for v in a {
                    Self(v.clone()).validate(depth + 1)?;
                }
            }
            serde_json::Value::Object(o) => {
                for v in o.values() {
                    Self(v.clone()).validate(depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ExactValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ExactValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bounded JSON")
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(ExactValue(serde_json::Value::Null))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(ExactValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v.unsigned_abs() > 9_007_199_254_740_991 {
                    return Err(E::custom("unsafe number"));
                }
                Ok(ExactValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v > 9_007_199_254_740_991 {
                    return Err(E::custom("unsafe number"));
                }
                Ok(ExactValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                let n =
                    serde_json::Number::from_f64(v).ok_or_else(|| E::custom("nonfinite number"))?;
                Ok(ExactValue(serde_json::Value::Number(n)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ExactValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(ExactValue(v.into()))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(ExactValue(v)) = a.next_element()? {
                    values.push(v);
                }
                Ok(ExactValue(serde_json::Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((k, ExactValue(v))) = a.next_entry::<String, ExactValue>()? {
                    if values.insert(k, v).is_some() {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                }
                Ok(ExactValue(serde_json::Value::Object(values)))
            }
        }
        d.deserialize_any(Visitor)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, TS)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum RoutineSchema {
    Null,
    Nullable {
        schema: Box<RoutineSchema>,
    },
    Boolean,
    Number {
        #[ts(type = "number")]
        min: serde_json::Number,
        #[ts(type = "number")]
        max: serde_json::Number,
    },
    String {
        #[serde(rename = "maxLength")]
        max_length: u32,
    },
    Array {
        #[serde(rename = "maxItems")]
        max_items: u32,
        items: Box<RoutineSchema>,
    },
    Object {
        properties: BTreeMap<String, RoutineSchema>,
        required: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineContract {
    pub schema_version: String,
    /// Exact dialect JSON. Shells render this closed descriptor, not JSON Schema.
    pub schema_json: RoutineJson,
    pub schema: RoutineSchema,
}

impl RoutineContract {
    pub fn from_wire(
        w: &waltr::grpc::waltr::routine::v2::ValueContract,
    ) -> Result<Self, ClientError> {
        if w.schema_version != "waltr.value/v1" {
            return Err(invalid());
        }
        let schema_json =
            RoutineJson::parse(String::from_utf8(w.schema_json.clone()).map_err(|_| invalid())?)?;
        validate_schema(&schema_json.value()?, 0, &mut 0)?;
        let schema = serde_json::from_value(schema_json.value()?).map_err(|_| invalid())?;
        Ok(Self {
            schema_version: w.schema_version.clone(),
            schema_json,
            schema,
        })
    }
    pub fn validate(&self, value: &RoutineJson) -> Result<(), ClientError> {
        if self.schema_version != "waltr.value/v1" {
            return Err(invalid());
        }
        let schema = self.schema_json.value()?;
        validate_schema(&schema, 0, &mut 0)?;
        matches_schema(&schema, &value.value()?)
    }
}

fn validate_schema(
    v: &serde_json::Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), ClientError> {
    *nodes += 1;
    if depth > 16 || *nodes > 256 {
        return Err(invalid());
    }
    let o = v.as_object().ok_or_else(invalid)?;
    let kind = o
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid)?;
    let fields: &[&str] = match kind {
        "null" | "boolean" => &["type"],
        "nullable" => {
            validate_schema(o.get("schema").ok_or_else(invalid)?, depth + 1, nodes)?;
            &["type", "schema"]
        }
        "number" => {
            let min = o
                .get("min")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(invalid)?;
            let max = o
                .get("max")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(invalid)?;
            if min > max {
                return Err(invalid());
            }
            &["type", "min", "max"]
        }
        "string" => {
            ceiling(o.get("maxLength"), 65536)?;
            &["type", "maxLength"]
        }
        "array" => {
            ceiling(o.get("maxItems"), 4096)?;
            validate_schema(o.get("items").ok_or_else(invalid)?, depth + 1, nodes)?;
            &["type", "maxItems", "items"]
        }
        "object" => {
            let properties = o
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(invalid)?;
            let required = o
                .get("required")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(invalid)?;
            if properties.len() > 128 {
                return Err(invalid());
            }
            let mut seen = std::collections::BTreeSet::new();
            for key in required {
                let key = key.as_str().ok_or_else(invalid)?;
                if !properties.contains_key(key) || !seen.insert(key) {
                    return Err(invalid());
                }
            }
            for child in properties.values() {
                validate_schema(child, depth + 1, nodes)?;
            }
            &["type", "properties", "required"]
        }
        _ => return Err(invalid()),
    };
    if o.len() != fields.len() || o.keys().any(|k| !fields.contains(&k.as_str())) {
        return Err(invalid());
    }
    Ok(())
}
fn ceiling(v: Option<&serde_json::Value>, max: u64) -> Result<u64, ClientError> {
    v.and_then(serde_json::Value::as_u64)
        .filter(|v| *v <= max)
        .ok_or_else(invalid)
}
fn matches_schema(s: &serde_json::Value, v: &serde_json::Value) -> Result<(), ClientError> {
    let ok = match s["type"].as_str().ok_or_else(invalid)? {
        "null" => v.is_null(),
        "nullable" => v.is_null() || matches_schema(&s["schema"], v).is_ok(),
        "boolean" => v.is_boolean(),
        "number" => v
            .as_f64()
            .is_some_and(|n| n >= s["min"].as_f64().unwrap() && n <= s["max"].as_f64().unwrap()),
        "string" => v
            .as_str()
            .is_some_and(|v| v.len() as u64 <= s["maxLength"].as_u64().unwrap()),
        "array" => v.as_array().is_some_and(|v| {
            v.len() as u64 <= s["maxItems"].as_u64().unwrap()
                && v.iter().all(|v| matches_schema(&s["items"], v).is_ok())
        }),
        "object" => v.as_object().is_some_and(|v| {
            let properties = s["properties"].as_object().unwrap();
            s["required"]
                .as_array()
                .unwrap()
                .iter()
                .all(|key| v.contains_key(key.as_str().unwrap()))
                && v.iter().all(|(k, v)| {
                    properties
                        .get(k)
                        .is_some_and(|s| matches_schema(s, v).is_ok())
                })
        }),
        _ => false,
    };
    if ok { Ok(()) } else { Err(invalid()) }
}
fn invalid() -> ClientError {
    ClientError::new("invalid bounded Routine value or contract")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_json_rejects_duplicate_keys_unsafe_numbers_and_trailing_data() {
        for v in [
            r#"{"x":1,"x":2}"#,
            "9007199254740993",
            "-9007199254740992",
            "1e999",
            "null true",
        ] {
            assert!(RoutineJson::parse(v).is_err(), "{v}");
        }
        assert_eq!(
            RoutineJson::parse("9007199254740991").unwrap().as_str(),
            "9007199254740991"
        );
    }
    #[test]
    fn nullable_closed_contract() {
        let c = RoutineContract::from_wire(&waltr::grpc::waltr::routine::v2::ValueContract {
            schema_version: "waltr.value/v1".into(),
            schema_json: br#"{"type":"object","properties":{"x":{"type":"nullable","schema":{"type":"string","maxLength":4}}},"required":["x"]}"#.to_vec(),
        }).unwrap();
        for v in [r#"{"x":null}"#, r#"{"x":"hi"}"#] {
            c.validate(&RoutineJson::parse(v).unwrap()).unwrap();
        }
        for v in [r#"{}"#, r#"{"x":"longer"}"#, r#"{"x":null,"unknown":true}"#] {
            assert!(c.validate(&RoutineJson::parse(v).unwrap()).is_err());
        }
    }
}
