//! Storage contracts for Waltr DataStores. Callers resolve authority before storage access.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Storage failure without backend secrets or content.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid datastore input")]
    Invalid,
    #[error("datastore item not found")]
    NotFound,
    #[error("datastore conflict")]
    Conflict,
    #[error("datastore limit exceeded")]
    Limit,
    #[error("datastore unavailable")]
    Unavailable,
    #[error("datastore outcome uncertain")]
    Uncertain,
}

/// Logical datastore type, independent of backend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Object,
    Relational,
}

/// Data partition policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Principal,
    Shared,
}

/// Configuration for partition resolution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub scope: Scope,
}

/// A logical store definition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Definition {
    #[serde(rename = "type")]
    pub kind: Kind,
    pub access: Access,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub collections: BTreeMap<String, Collection>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relations: BTreeMap<String, Relation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    pub fields: BTreeMap<String, Field>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    #[serde(rename = "type")]
    pub kind: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub indexed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Integer,
    Number,
    Boolean,
    Json,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Relation {
    pub from: String,
    pub to: String,
    pub cardinality: Cardinality,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cardinality {
    OneToOne,
    OneToMany,
    ManyToMany,
}

/// Validate a bounded logical identifier.
pub fn identifier(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > 128 || !value.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')) {
        return Err(Error::Invalid);
    }
    Ok(())
}

impl Definition {
    /// Validate cross-references and resource limits.
    pub fn validate(&self) -> Result<(), Error> {
        if self.collections.len() > 64 || self.relations.len() > 128 {
            return Err(Error::Limit);
        }
        if (self.kind == Kind::Object && (!self.collections.is_empty() || !self.relations.is_empty()))
            || (self.kind == Kind::Relational && self.collections.is_empty()) {
            return Err(Error::Invalid);
        }
        for (name, collection) in &self.collections {
            identifier(name)?;
            if collection.fields.len() > 128 {
                return Err(Error::Limit);
            }
            for (name, field) in &collection.fields {
                identifier(name)?;
                if field.indexed && field.kind == FieldType::Json {
                    return Err(Error::Invalid);
                }
            }
        }
        for (name, relation) in &self.relations {
            identifier(name)?;
            if !self.collections.contains_key(&relation.from) || !self.collections.contains_key(&relation.to) {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
}

/// Opaque trusted actor key.
#[derive(Clone, Eq, PartialEq)]
pub struct Actor(String);

impl Actor {
    pub fn from_trusted_key(key: String) -> Result<Self, Error> {
        identifier(&key)?;
        Ok(Self(key))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A backend partition, derived only from a validated store and an authorized actor.
#[derive(Clone, Eq, PartialEq)]
pub struct Namespace(String);

impl Namespace {
    pub fn resolve(name: &str, definition: &Definition, actor: Option<&Actor>) -> Result<Self, Error> {
        identifier(name)?;
        let partition = match definition.access.scope {
            Scope::Shared => "shared".to_owned(),
            Scope::Principal => {
                let actor = actor.ok_or(Error::NotFound)?;
                format!("principal-{}", Sha256::digest(actor.as_str()).iter()
                    .map(|byte| format!("{byte:02x}")).collect::<String>())
            }
        };
        let kind = match definition.kind {
            Kind::Object => "object",
            Kind::Relational => "relational",
        };
        Ok(Self(format!("{kind}/{name}/{partition}")))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validate an object key common to every backend.
pub fn object_key(key: &str) -> Result<(), Error> {
    if key.len() > 768 || key.split('/').any(|part| {
        part.is_empty() || part.len() > 240 || matches!(part, "." | "..") || part.ends_with(['.', ' '])
            || part.chars().any(|c| c.is_control() || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
            || {
                let name = part.split('.').next().unwrap_or_default().to_ascii_uppercase();
                matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    || ["COM", "LPT"].iter().any(|prefix| name.strip_prefix(prefix).is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")))
            }
    }) {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// Authorship that the backend can substantiate for the current record.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attribution {
    pub created_by: Option<String>,
    pub last_edited_by: Option<String>,
}

/// Maximum bytes for one object. Transports can impose tighter limits.
pub const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum results per page.
pub const MAX_PAGE: usize = 100;

/// Direct object access to the configured filesystem or S3 backend.
pub mod object;
/// Native relational access to the configured SQLite or PostgreSQL backend.
pub mod relational;
