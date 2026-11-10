//! Strict JavaScript definitions and declarative invocation contracts.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use waltr_script::{Limits, ScriptEngine, StructuredValue};

/// Sole admitted program-envelope version.
pub const API_VERSION: &str = "waltr.routine/v1";
/// Closed value-contract dialect; not JSON Schema.
pub const SCHEMA_VERSION: &str = "waltr.value/v1";
/// Maximum raw YAML and canonical JSON envelope bytes.
pub const MAX_ENVELOPE_BYTES: usize = 131_072;

#[derive(Clone, Debug, PartialEq, Eq)]
/// Trusted validation classification; excludes source text and input values.
pub struct DefinitionError(pub &'static str);
impl std::fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for DefinitionError {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Strict JavaScript resource envelope.
pub struct Envelope {
    /// Exact supported API identity.
    pub api_version: String,
    /// Must equal `Routine`.
    pub kind: String,
    /// Bounded catalog identity.
    pub name: String,
    /// Human-readable description, at most 4096 bytes.
    pub description: String,
    /// Complete inline program and its contracts.
    pub spec: Program,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Declarative contracts around one complete ES module.
pub struct Program {
    /// Exact source, never a path, task list or expression.
    pub source: String,
    /// Adapter descriptors, at most 32.
    pub entrypoints: BTreeMap<String, Entrypoint>,
    /// Complete input-value contract.
    pub parameters: Contract,
    /// Complete output-value contract.
    pub result: Contract,
    /// Declarative media contracts; adapters supply media I/O.
    #[serde(default)]
    pub ports: BTreeMap<String, Port>,
    /// Required capabilities, checked together by the supplied host factory.
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    /// Resource-owned capture contracts bound to exact helper selections.
    #[serde(default)]
    pub captures: BTreeMap<String, CaptureContract>,
    /// Captured per-invocation ceilings.
    #[serde(default)]
    pub budgets: Budgets,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Declarative trigger identity, not an execution graph.
pub struct Entrypoint {
    /// Adapter family.
    pub trigger: Trigger,
    /// Semantic adapter selector, never code or an expression.
    #[serde(default)]
    pub selector: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Supported descriptor families; adapters remain separate integrations.
pub enum Trigger {
    /// Explicit manual request.
    Manual,
    /// Event subscription.
    Event,
    /// Interactive lease.
    Interactive,
    /// Tool-owned request.
    Tool,
    /// Explicit inline tool-owned request.
    Inline,
    /// Explicit MCP-owned request.
    Mcp,
    /// Scheduled request.
    Schedule,
    /// Progress subscription.
    Progress,
    /// Lifecycle request.
    Lifecycle,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Bounded typed-media metadata; does not grant resource access.
pub struct Port {
    /// Input or output relative to the invocation.
    pub direction: Direction,
    /// Media family.
    pub media: Media,
    /// Whether admission requires an adapter for this port.
    pub required: bool,
    /// Aggregate payload ceiling, separate from the 64 KiB transport chunk ceiling.
    pub max_bytes: u32,
    /// Allowed format identities, at most 16 strings of 128 bytes.
    pub formats: Vec<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Port direction relative to the invocation.
pub enum Direction {
    /// Consumed by the invocation.
    Input,
    /// Produced by the invocation.
    Output,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Media metadata families; no transport or codec behavior.
pub enum Media {
    /// Audio content.
    Audio,
    /// Text content.
    Text,
    /// Image content.
    Image,
    /// Generic file content.
    File,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Per-invocation budgets; all other engine limits retain their defaults.
pub struct Budgets {
    /// Maximum complete encoded value bytes, at most 65536.
    pub value_bytes: usize,
    /// Maximum complete result bytes, no greater than value_bytes.
    pub result_bytes: usize,
    /// Total host calls, at most 1024.
    pub host_calls: usize,
    /// Concurrent host calls, at most 32.
    pub in_flight: usize,
    /// Engine deadline; media profiles may explicitly request up to 90 minutes.
    pub deadline_ms: u64,
}
impl Default for Budgets {
    fn default() -> Self {
        Self {
            value_bytes: 65_536,
            result_bytes: 65_536,
            host_calls: 256,
            in_flight: 16,
            deadline_ms: 30_000,
        }
    }
}
impl Budgets {
    /// Validate the exposed budgets and capture immutable engine limits.
    pub fn limits(&self) -> Result<Limits, DefinitionError> {
        let limits = Limits {
            value_bytes: self.value_bytes,
            host_calls: self.host_calls,
            in_flight: self.in_flight,
            deadline: Duration::from_millis(self.deadline_ms),
            ..Limits::default()
        };
        if self.result_bytes == 0
            || self.result_bytes > self.value_bytes
            || self.deadline_ms > 5_400_000
        {
            return Err(DefinitionError("invalid Routine budget"));
        }
        limits
            .validate()
            .map_err(|_| DefinitionError("invalid engine budget"))?;
        Ok(limits)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Explicit value dialect and closed schema.
pub struct Contract {
    /// Must equal `waltr.value/v1`.
    pub schema_version: String,
    /// Closed, bounded complete-value shape.
    pub schema: Schema,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// A capture declaration does not grant permission to execute its tool.
pub struct CaptureContract {
    /// Exact named or restricted helper that may submit this contract.
    pub selection: super::ports::HelperSelection,
    /// Bounded supported value dialect.
    pub contract: Contract,
}
/// Closed, bounded value schema, not JSON Schema. Unsupported keywords are errors.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Schema {
    /// JSON null.
    Null,
    /// Null or the enclosed bounded schema; the wrapper consumes one schema node.
    Nullable {
        /// Non-null values must match this schema.
        schema: Box<Schema>,
    },
    /// JSON boolean.
    Boolean,
    /// Finite bounded number, subject to the engine's exact-number rules.
    Number {
        /// Inclusive lower bound.
        min: f64,
        /// Inclusive upper bound.
        max: f64,
    },
    /// UTF-8 byte-bounded string.
    String {
        /// Maximum UTF-8 bytes, not character count.
        #[serde(rename = "maxLength")]
        max_length: usize,
    },
    /// Homogeneous bounded array.
    Array {
        /// Maximum element count.
        #[serde(rename = "maxItems")]
        max_items: usize,
        /// Every element's schema.
        items: Box<Schema>,
    },
    /// Closed object; undeclared keys always fail.
    Object {
        /// Allowed keys and schemas.
        properties: BTreeMap<String, Schema>,
        /// Required keys, all present in properties.
        required: BTreeSet<String>,
    },
}
impl Contract {
    /// Validate host-owned captures without admitting them as engine values.
    /// The caller pins this consumer bound; no engine limit is changed.
    pub fn accepts_host(&self, value: &Value, max_encoded_bytes: usize) -> bool {
        if max_encoded_bytes <= 65536 && self.accepts(value, &Limits::default()).is_err() {
            return false;
        }
        struct Counter {
            used: usize,
            limit: usize,
        }
        impl std::io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.used = self
                    .used
                    .checked_add(bytes.len())
                    .filter(|n| *n <= self.limit)
                    .ok_or_else(|| std::io::Error::other("capture limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        max_encoded_bytes > 0
            && max_encoded_bytes <= 32 * 1024 * 1024
            && self.schema_version == SCHEMA_VERSION
            && self.schema.accepts(value)
            && serde_json::to_writer(
                Counter {
                    used: 0,
                    limit: max_encoded_bytes,
                },
                value,
            )
            .is_ok()
    }
    pub(crate) fn validate(&self) -> Result<(), DefinitionError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(DefinitionError("unsupported value schema version"));
        }
        let mut nodes = 0;
        self.schema.check(0, &mut nodes, 65536)
    }
    pub(crate) fn validate_capture(&self) -> Result<(), DefinitionError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(DefinitionError("unsupported value schema version"));
        }
        let mut nodes = 0;
        self.schema.check(0, &mut nodes, 1048576)
    }
    /// Validate the complete value against engine bounds and the declared shape.
    pub fn accepts(
        &self,
        value: &Value,
        limits: &Limits,
    ) -> Result<StructuredValue, DefinitionError> {
        let bounded = StructuredValue::new(value.clone(), limits)
            .map_err(|_| DefinitionError("value budget exceeded"))?;
        if !self.schema.accepts(value) {
            return Err(DefinitionError("value does not match declared contract"));
        }
        Ok(bounded)
    }
}
impl Schema {
    fn check(
        &self,
        depth: usize,
        nodes: &mut usize,
        string_limit: usize,
    ) -> Result<(), DefinitionError> {
        *nodes += 1;
        if depth > 16 || *nodes > 256 {
            return Err(DefinitionError("schema budget exceeded"));
        }
        match self {
            Self::Nullable { schema } => schema.check(depth + 1, nodes, string_limit)?,
            Self::Number { min, max } if !min.is_finite() || !max.is_finite() || min > max => {
                return Err(DefinitionError("invalid number range"));
            }
            Self::String { max_length } if *max_length > string_limit => {
                return Err(DefinitionError("invalid string bound"));
            }
            Self::Array { max_items, items } => {
                if *max_items > 4096 {
                    return Err(DefinitionError("invalid array bound"));
                }
                items.check(depth + 1, nodes, string_limit)?;
            }
            Self::Object {
                properties,
                required,
            } => {
                if properties.len() > 128
                    || !required.iter().all(|key| properties.contains_key(key))
                {
                    return Err(DefinitionError("invalid object contract"));
                }
                for (key, schema) in properties {
                    if key.len() > 128 {
                        return Err(DefinitionError("schema key too long"));
                    }
                    schema.check(depth + 1, nodes, string_limit)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn accepts(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Nullable { schema }, value) => value.is_null() || schema.accepts(value),
            (Self::Null, Value::Null) | (Self::Boolean, Value::Bool(_)) => true,
            (Self::Number { min, max }, Value::Number(n)) => {
                n.as_f64().is_some_and(|n| n >= *min && n <= *max)
            }
            (Self::String { max_length }, Value::String(s)) => s.len() <= *max_length,
            (Self::Array { max_items, items }, Value::Array(values)) => {
                values.len() <= *max_items && values.iter().all(|v| items.accepts(v))
            }
            (
                Self::Object {
                    properties,
                    required,
                },
                Value::Object(values),
            ) => {
                required.iter().all(|k| values.contains_key(k))
                    && values
                        .iter()
                        .all(|(k, v)| properties.get(k).is_some_and(|s| s.accepts(v)))
            }
            _ => false,
        }
    }
}

/// Immutable validated snapshot. Only this constructor can issue revisions.
#[derive(Clone)]
pub struct Definition {
    envelope: Envelope,
    canonical: String,
    revision: String,
    engine: String,
    limits: Limits,
}
impl Definition {
    /// Decode strict YAML, validate bounded contracts, then compile without evaluation.
    pub fn parse(yaml: &str, engine: &dyn ScriptEngine) -> Result<Arc<Self>, DefinitionError> {
        if yaml.len() > MAX_ENVELOPE_BYTES {
            return Err(DefinitionError("envelope budget exceeded"));
        }
        let envelope: Envelope = serde_yaml_ng::from_str(yaml)
            .map_err(|_| DefinitionError("invalid or unsupported Routine envelope field"))?;
        if envelope.api_version != API_VERSION || envelope.kind != "Routine" {
            return Err(DefinitionError("unsupported Routine API version or kind"));
        }
        if !identifier(&envelope.name) || envelope.description.len() > 4096 {
            return Err(DefinitionError("invalid Routine identity"));
        }
        let spec = &envelope.spec;
        if spec.budgets.deadline_ms > 30_000 && !spec.capabilities.contains("media.execute") {
            return Err(DefinitionError("long deadline requires media preflight"));
        }
        if spec.entrypoints.is_empty()
            || spec.entrypoints.len() > 32
            || spec.ports.len() > 32
            || spec.capabilities.len() > 64
        {
            return Err(DefinitionError("declaration budget exceeded"));
        }
        for (name, entry) in &spec.entrypoints {
            if !identifier(name) || entry.selector.as_ref().is_some_and(|s| !identifier(s)) {
                return Err(DefinitionError("invalid entrypoint descriptor"));
            }
        }
        for (name, port) in &spec.ports {
            if !identifier(name)
                || port.max_bytes == 0
                || u64::from(port.max_bytes)
                    > match port.media {
                        Media::Audio => 256 * 1024 * 1024,
                        Media::Text => 1024 * 1024,
                        Media::Image => 20 * 1024 * 1024,
                        Media::File => 64 * 1024 * 1024,
                    }
                || port.formats.is_empty()
                || port.formats.len() > 16
                || port.formats.iter().any(|s| s.is_empty() || s.len() > 128)
            {
                return Err(DefinitionError("invalid media metadata"));
            }
        }
        if spec.capabilities.iter().any(|s| !identifier(s)) {
            return Err(DefinitionError("invalid capability identity"));
        }
        spec.parameters.validate()?;
        spec.result.validate()?;
        if spec.captures.len() > 32 {
            return Err(DefinitionError("capture declaration budget exceeded"));
        }
        for (name, capture) in &spec.captures {
            let capability = match &capture.selection {
                super::ports::HelperSelection::Named { name } => format!("helper.named.{name}"),
                super::ports::HelperSelection::Restricted { profile } => {
                    format!("helper.restricted.{profile}")
                }
            };
            if !identifier(name)
                || !spec.capabilities.contains(&capability)
                || !spec
                    .capabilities
                    .contains(&format!("helper.capture.{name}"))
                || serde_json::to_vec(capture).map_or(true, |bytes| bytes.len() > 16384)
            {
                return Err(DefinitionError("invalid capture declaration"));
            }
            capture.contract.validate_capture()?;
        }
        if spec
            .capabilities
            .iter()
            .filter_map(|cap| cap.strip_prefix("helper.capture."))
            .any(|name| !spec.captures.contains_key(name))
        {
            return Err(DefinitionError("missing capture declaration"));
        }
        let limits = spec.budgets.limits()?;
        engine
            .validate(&spec.source, &limits)
            .map_err(|_| DefinitionError("program validation failed"))?;
        let canonical = serde_json::to_string(&envelope)
            .map_err(|_| DefinitionError("envelope encoding failed"))?;
        if canonical.len() > MAX_ENVELOPE_BYTES {
            return Err(DefinitionError("canonical envelope budget exceeded"));
        }
        let info = engine.info();
        let engine = format!(
            "{};{};abi={}",
            info.javascript, info.interpreter, info.api_version
        );
        let revision = revision_id(&engine, &canonical);
        Ok(Arc::new(Self {
            envelope,
            canonical,
            revision,
            engine,
            limits,
        }))
    }
    /// Read the immutable admitted program contract.
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }
    /// Canonical JSON envelope, with the exact decoded source.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }
    /// SHA-256 over API identity, engine identity and canonical envelope.
    pub fn revision(&self) -> &str {
        &self.revision
    }
    /// Engine language, interpreter and host ABI identity.
    pub fn engine(&self) -> &str {
        &self.engine
    }
    /// Captured immutable limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
}
impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Definition")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
pub(crate) fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
pub(crate) fn revision_id(engine: &str, canonical: &str) -> String {
    Sha256::digest(format!("{}\0{engine}\0{canonical}", API_VERSION))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod nullable_tests {
    use super::*;
    use serde_json::json;

    fn contract(schema: Value) -> Contract {
        serde_json::from_value(json!({"schemaVersion":SCHEMA_VERSION,"schema":schema})).unwrap()
    }
    #[test]
    fn capture_contract_is_resource_data_bounded_before_admission() {
        let engine = waltr_script::Engine::new(1).unwrap();
        let mut envelope: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/routines/title.json")).unwrap();
        envelope["spec"]["capabilities"] = json!([
            "helper.run",
            "helper.named.editor-a",
            "helper.capture.output"
        ]);
        envelope["spec"]["captures"] = json!({"output":{
            "selection":{"kind":"named","name":"editor-a"},
            "contract":{"schemaVersion":SCHEMA_VERSION,"schema":{"type":"object","properties":{"chosen-key":{"type":"string","maxLength":1048576}},"required":["chosen-key"]}}
        }});
        let first = Definition::parse(&envelope.to_string(), &engine).unwrap();
        let capture = &first.envelope().spec.captures["output"];
        assert!(
            capture
                .contract
                .accepts_host(&json!({"chosen-key":"kept"}), 1048576)
        );
        assert!(
            !capture
                .contract
                .accepts_host(&json!({"other":"kept"}), 1048576)
        );
        envelope["spec"]["capabilities"] = json!([
            "helper.run",
            "helper.named.editor-b",
            "helper.capture.output"
        ]);
        envelope["spec"]["captures"]["output"]["selection"]["name"] = json!("editor-b");
        envelope["spec"]["captures"]["output"]["contract"]["schema"]["properties"]["chosen-key"] =
            json!({"type":"boolean"});
        let edited = Definition::parse(&envelope.to_string(), &engine).unwrap();
        assert_ne!(first.revision(), edited.revision());
        assert!(
            edited.envelope().spec.captures["output"]
                .contract
                .accepts_host(&json!({"chosen-key":true}), 65536)
        );
        envelope["spec"]["captures"]["output"]["selection"]["name"] = json!("foreign");
        assert!(Definition::parse(&envelope.to_string(), &engine).is_err());
        envelope["spec"]["captures"]["output"]["selection"]["name"] = json!("editor-b");
        envelope["spec"]["captures"]["output"]["contract"]["schema"]["properties"]["chosen-key"] =
            json!({"type":"string","maxLength":1048577});
        assert!(Definition::parse(&envelope.to_string(), &engine).is_err());
        envelope["spec"]["captures"]["output"]["contract"]["schema"] =
            json!({"type":"object","properties":{},"required":[],"additionalProperties":true});
        assert!(Definition::parse(&envelope.to_string(), &engine).is_err());
    }

    #[test]
    fn nullable_is_closed_bounded_and_does_not_make_required_keys_optional() {
        let contract = contract(json!({"type":"object","properties":{"maskId":{
            "type":"nullable","schema":{"type":"string","maxLength":4}
        }},"required":["maskId"]}));
        contract.validate().unwrap();
        let limits = Limits::default();
        for value in [json!({"maskId":null}), json!({"maskId":"mask"})] {
            contract.accepts(&value, &limits).unwrap();
        }
        for value in [
            json!({}),
            json!({"maskId":true}),
            json!({"maskId":3}),
            json!({"maskId":[]}),
            json!({"maskId":"oversized"}),
            json!({"maskId":null,"extra":1}),
        ] {
            assert!(contract.accepts(&value, &limits).is_err(), "{value}");
        }
        for schema in [
            json!({"type":"nullable","schema":{"type":"null"},"extra":true}),
            json!({"type":"nullable","schema":{"type":"string","maxLength":4,"extra":true}}),
            json!({"type":"nullable"}),
        ] {
            assert!(serde_json::from_value::<Schema>(schema).is_err());
        }
    }

    #[test]
    fn nullable_wrapper_and_child_share_depth_and_node_budgets() {
        let mut schema = json!({"type":"null"});
        for _ in 0..16 {
            schema = json!({"type":"nullable","schema":schema});
        }
        contract(schema.clone()).validate().unwrap();
        assert!(
            contract(json!({"type":"nullable","schema":schema}))
                .validate()
                .is_err()
        );
        let properties = (0..127)
            .map(|i| {
                (
                    format!("k{i}"),
                    json!({"type":"nullable","schema":{"type":"null"}}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let mut schema = json!({"type":"object","properties":properties,"required":[]});
        contract(schema.clone()).validate().unwrap();
        schema["properties"]["extra"] = json!({"type":"null"});
        contract(schema.clone()).validate().unwrap();
        schema["properties"]["extra"] = json!({"type":"nullable","schema":{"type":"null"}});
        assert!(contract(schema).validate().is_err());
        assert!(
            contract(json!({"type":"nullable","schema":{"type":"string","maxLength":65537}}))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn nullable_keeps_complete_value_bounds_and_canonical_revision() {
        let contract =
            contract(json!({"type":"nullable","schema":{"type":"string","maxLength":65536}}));
        contract.validate().unwrap();
        assert!(
            contract
                .accepts(&json!("x".repeat(65536)), &Limits::default())
                .is_err()
        );
        let engine = waltr_script::Engine::new(1).unwrap();
        let mut envelope: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/routines/title.json")).unwrap();
        envelope["spec"]["result"]["schema"] =
            json!({"type":"nullable","schema":{"type":"string","maxLength":128}});
        let first = Definition::parse(&serde_json::to_string(&envelope).unwrap(), &engine).unwrap();
        let second =
            Definition::parse(&serde_json::to_string_pretty(&envelope).unwrap(), &engine).unwrap();
        assert_eq!(first.canonical(), second.canonical());
        assert_eq!(first.revision(), second.revision());
        envelope["spec"]["result"]["schema"] = json!({"type":"string","maxLength":128});
        let different =
            Definition::parse(&serde_json::to_string(&envelope).unwrap(), &engine).unwrap();
        assert_ne!(first.revision(), different.revision());
    }
}
