//! Closed wire/domain conversion; no execution or capability inference.
use super::catalog::{PublicDefinition, StartContext};
use crate::routine::{
    definition as d, history as h,
    observation::{InvocationView, OperationView, Publication},
    ports::PortError,
    text_results::TextResultDescriptor,
};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::fmt;
use tonic::Status;
use waltr::grpc::waltr::routine::v2 as p;

pub(super) fn error(error: PortError) -> Status {
    match error {
        PortError::Invalid => Status::invalid_argument("invalid routine request"),
        PortError::Denied => Status::permission_denied("routine access denied"),
        PortError::Conflict => Status::failed_precondition("routine state changed"),
        PortError::Busy => Status::resource_exhausted("routine already running"),
        PortError::UnsupportedCapability => {
            Status::failed_precondition("routine capability unavailable")
        }
        PortError::Unavailable | PortError::Uncertain => {
            Status::unavailable("routine owner unavailable")
        }
    }
}
pub(super) fn id(value: &str, max: usize) -> Result<(), Status> {
    if value.is_empty() || value.len() > max {
        Err(Status::invalid_argument("invalid identity"))
    } else {
        Ok(())
    }
}
pub(super) fn cursor(value: u64) -> Result<i64, Status> {
    i64::try_from(value).map_err(|_| Status::invalid_argument("invalid cursor"))
}
pub(super) fn structured(value: Option<p::StructuredValue>) -> Result<Value, Status> {
    let value = value.ok_or_else(|| Status::invalid_argument("missing structured value"))?;
    if value.json.len() > 65536 {
        return Err(Status::invalid_argument("structured value too large"));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&value.json);
    let result = Strict(0)
        .deserialize(&mut deserializer)
        .map_err(|_| Status::invalid_argument("invalid structured value"))?;
    deserializer
        .end()
        .map_err(|_| Status::invalid_argument("invalid structured value"))?;
    waltr_script::StructuredValue::new(result.clone(), &waltr_script::Limits::default())
        .map_err(|_| Status::invalid_argument("unsupported structured value"))?;
    Ok(result)
}
struct Strict(usize);
impl<'de> DeserializeSeed<'de> for Strict {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        if self.0 > 32 {
            return Err(de::Error::custom("depth"));
        }
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Strict {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded exact JSON")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("number"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = a.next_element_seed(Strict(self.0 + 1))? {
            if values.len() == 4096 {
                return Err(de::Error::custom("nodes"));
            }
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = a.next_key::<String>()? {
            if values.contains_key(&key) || values.len() == 4096 {
                return Err(de::Error::custom("duplicate or nodes"));
            }
            values.insert(key, a.next_value_seed(Strict(self.0 + 1))?);
        }
        Ok(Value::Object(values))
    }
}
fn value(value: Value) -> Result<p::StructuredValue, Status> {
    Ok(p::StructuredValue {
        json: serde_json::to_vec(&value)
            .map_err(|_| Status::internal("invalid durable projection"))?,
    })
}
pub(super) fn trigger(value: d::Trigger) -> i32 {
    match value {
        d::Trigger::Manual => 1,
        d::Trigger::Event => 2,
        d::Trigger::Interactive => 3,
        d::Trigger::Tool => 4,
        d::Trigger::Inline => 5,
        d::Trigger::Mcp => 6,
        d::Trigger::Schedule => 7,
        d::Trigger::Progress => 8,
        d::Trigger::Lifecycle => 9,
    }
}
fn contract(value: d::Contract) -> Result<p::ValueContract, Status> {
    Ok(p::ValueContract {
        schema_version: value.schema_version,
        schema_json: serde_json::to_vec(&value.schema)
            .map_err(|_| Status::internal("invalid contract"))?,
    })
}
pub(super) fn definition(value: PublicDefinition) -> Result<p::Definition, Status> {
    let result = p::Definition {
        name: value.name,
        description: value.description,
        revision: value.revision,
        api_version: value.api_version,
        engine: value.engine,
        entrypoints: value
            .entrypoints
            .into_iter()
            .map(|(name, entry)| p::Entrypoint {
                start_contexts: value
                    .offers
                    .get(&name)
                    .into_iter()
                    .flatten()
                    .map(|c| match c {
                        StartContext::Global => 1,
                        StartContext::Session => 2,
                    })
                    .collect(),
                name,
                trigger: trigger(entry.trigger),
                selector: entry.selector,
            })
            .collect(),
        parameters: Some(contract(value.parameters)?),
        result: Some(contract(value.result)?),
        ports: value
            .ports
            .into_iter()
            .map(|(name, port)| p::Port {
                name,
                direction: match port.direction {
                    d::Direction::Input => 1,
                    d::Direction::Output => 2,
                },
                media: match port.media {
                    d::Media::Audio => 1,
                    d::Media::Text => 2,
                    d::Media::Image => 3,
                    d::Media::File => 4,
                },
                required: port.required,
                max_bytes: u64::from(port.max_bytes),
                formats: port.formats,
            })
            .collect(),
        capabilities: value.capabilities.into_iter().collect(),
        budgets: Some(p::Budgets {
            value_bytes: value.budgets.value_bytes as u32,
            result_bytes: value.budgets.result_bytes as u32,
            host_calls: value.budgets.host_calls as u32,
            in_flight: value.budgets.in_flight as u32,
            deadline_ms: value.budgets.deadline_ms,
        }),
    };
    Ok(result)
}
pub(super) fn text(value: TextResultDescriptor) -> p::TextResultDescriptor {
    p::TextResultDescriptor {
        version: value.version,
        id: value.id,
        name: value.name,
        sha256: value.sha256,
        utf8_bytes: value.utf8_bytes,
        encoded_charge: value.encoded_charge,
        helper_id: value.helper_id,
    }
}
pub(super) fn pinned(view: &InvocationView) -> Result<PublicDefinition, Status> {
    match &view.public_definition {
        Some(projection) => serde_json::from_value(projection.clone())
            .map_err(|_| Status::internal("invalid durable public definition")),
        None => PublicDefinition::historical(&view.revision).map_err(error),
    }
}
pub(super) fn invocation(view: InvocationView) -> Result<p::Invocation, Status> {
    let mut helpers = Vec::new();
    let mut binary_results = Vec::new();
    for publication in view.publications {
        match publication {
            Publication::Helper {
                session_id,
                definition_identity,
            } => helpers.push(p::HelperReference {
                session_id,
                definition_identity,
                operation_id: None,
            }),
            Publication::Blob {
                blob_id,
                display_name,
                content_type,
                bytes,
                sha256,
                image,
                remaining_validity_ms,
                ..
            } => binary_results.push(p::BlobDescriptor {
                blob_id,
                metadata: Some(p::BlobMetadata {
                    display_name,
                    content_type,
                    bytes,
                    sha256: sha256.to_vec(),
                    image: image.map(|(format, width, height)| p::ImageInfo {
                        format: format as i32,
                        width,
                        height,
                    }),
                }),
                remaining_validity_ms: Some(remaining_validity_ms),
            }),
        }
    }
    let i = view.invocation;
    Ok(p::Invocation {
        version: i.version,
        cursor: i.cursor as u64,
        id: i.id,
        routine: view.revision.name,
        revision: i.revision,
        entrypoint: i.entrypoint,
        origin: match i.caller.origin {
            h::Origin::Manual => 1,
            h::Origin::Event => 2,
            h::Origin::Interactive => 3,
            h::Origin::Tool => 4,
            h::Origin::Inline => 5,
            h::Origin::Mcp => 6,
            h::Origin::Schedule => 7,
            h::Origin::Progress => 8,
            h::Origin::Lifecycle => 9,
        },
        ownership: match i.caller.ownership {
            h::Ownership::Detached => 1,
            h::Ownership::Leased => 2,
            h::Ownership::Tool => 3,
        },
        correlation: i.caller.correlation,
        snapshot: i.snapshot,
        status: match i.status {
            h::Status::Admitted => 1,
            h::Status::Running => 2,
            h::Status::Succeeded => 3,
            h::Status::Failed => 4,
            h::Status::Interrupted => 5,
            h::Status::Uncertain => 6,
        },
        admitted_ms: i.admitted_ms,
        updated_ms: i.updated_ms,
        result: i.result.map(value).transpose()?,
        text_results: view.text_results.into_iter().map(text).collect(),
        binary_results,
        helpers,
        observation_revision: view.observation_revision as u64,
        source_session: view.source_session,
    })
}
pub(super) fn operation(view: OperationView) -> Result<p::Operation, Status> {
    let o = view.operation;
    Ok(p::Operation {
        version: o.version,
        cursor: o.cursor as u64,
        id: o.id,
        invocation_id: o.invocation,
        name: o.name,
        status: match o.status {
            h::OperationStatus::Dispatched => 1,
            h::OperationStatus::Completed => 2,
            h::OperationStatus::Failed => 3,
            h::OperationStatus::Uncertain => 4,
        },
        arguments: Some(value(o.arguments)?),
        result: o.result.map(value).transpose()?,
        updated_ms: o.updated_ms,
        observation_revision: view.observation_revision as u64,
    })
}
