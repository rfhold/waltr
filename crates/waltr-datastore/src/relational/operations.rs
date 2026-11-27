use serde_json::Value;

use super::{
    Direction, Fields, Link, LinkedRecord, Native, Page, Query, Record, Traversal, execute,
    query::{self, Cursor, MAX_TEXT_BYTES, Position},
    run, sql_error,
};
use crate::{
    Actor, Attribution, Cardinality, Collection, Definition, Error, FieldType, Kind, Namespace,
    Relation, Scope, identifier,
};

const MAX_FIELDS_BYTES: usize = 64 * 1024;
const MAX_JSON_DEPTH: usize = 48;

type RecordRow = (String, String, Option<String>, Option<String>);
type FindRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<f64>,
    Option<i64>,
    i64,
);
type LinkRow = (String, String, Option<String>, Option<String>);
type LinkedRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn scope(
    namespace: &Namespace,
    definition: &Definition,
    actor: Option<&Actor>,
) -> Result<(), Error> {
    let mut parts = namespace.as_str().split('/');
    if definition.kind != Kind::Relational || parts.next() != Some("relational") {
        return Err(Error::Invalid);
    }
    let name = parts.next().ok_or(Error::Invalid)?;
    match (definition.access.scope, parts.next()) {
        (Scope::Shared, Some("shared")) => {}
        (Scope::Principal, Some(partition)) if partition.starts_with("principal-") => {
            if let Some(actor) = actor
                && &Namespace::resolve(name, definition, Some(actor))? != namespace
            {
                return Err(Error::NotFound);
            }
        }
        _ => return Err(Error::Invalid),
    }
    Ok(())
}

fn collection<'a>(definition: &'a Definition, name: &str) -> Result<&'a Collection, Error> {
    definition.collections.get(name).ok_or(Error::NotFound)
}

fn relation<'a>(definition: &'a Definition, name: &str) -> Result<&'a Relation, Error> {
    definition.relations.get(name).ok_or(Error::NotFound)
}

fn json(value: &Value, depth: usize) -> Result<(), Error> {
    if depth > MAX_JSON_DEPTH {
        return Err(Error::Limit);
    }
    match value {
        Value::String(value) if value.contains('\0') => return Err(Error::Invalid),
        Value::Array(values) => {
            for value in values {
                json(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if key.contains('\0') {
                    return Err(Error::Invalid);
                }
                json(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_fields(fields: &Fields, schema: &Collection) -> Result<(), Error> {
    for (name, value) in fields {
        let spec = schema.fields.get(name).ok_or(Error::Invalid)?;
        if value.is_null() && !spec.required {
            continue;
        }
        let valid = match spec.kind {
            FieldType::String => value
                .as_str()
                .is_some_and(|value| value.len() <= MAX_TEXT_BYTES && !value.contains('\0')),
            FieldType::Integer => value.as_i64().is_some(),
            FieldType::Number => value.as_f64().is_some_and(f64::is_finite),
            FieldType::Boolean => value.is_boolean(),
            FieldType::Json => {
                json(value, 0)?;
                !value.is_null()
            }
        };
        if !valid {
            return Err(Error::Invalid);
        }
    }
    if schema
        .fields
        .iter()
        .any(|(name, field)| field.required && !fields.contains_key(name))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

fn encode(fields: &Fields, schema: &Collection) -> Result<String, Error> {
    validate_fields(fields, schema)?;
    let value = serde_json::to_string(fields).map_err(|_| Error::Invalid)?;
    if value.len() > MAX_FIELDS_BYTES {
        return Err(Error::Limit);
    }
    Ok(value)
}

fn decode(row: RecordRow, schema: &Collection) -> Result<Record, Error> {
    let (id, fields, created_by, last_edited_by) = row;
    let fields: Fields = serde_json::from_str(&fields).map_err(|_| Error::Invalid)?;
    // Check canonical JSON size rather than PostgreSQL's presentation whitespace.
    encode(&fields, schema)?;
    identifier(&id)?;
    Ok(Record {
        id,
        fields,
        attribution: Attribution {
            created_by,
            last_edited_by,
        },
    })
}

fn link_row(row: LinkRow) -> Link {
    let (from_id, to_id, created_by, last_edited_by) = row;
    Link {
        from_id,
        to_id,
        attribution: Attribution {
            created_by,
            last_edited_by,
        },
    }
}

pub(super) async fn get(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    name: &str,
    id: &str,
) -> Result<Record, Error> {
    scope(namespace, definition, None)?;
    identifier(id)?;
    let schema = collection(definition, name)?;
    let row = run!(
        native,
        "SELECT id, CAST(fields AS TEXT), created_by, last_edited_by \
         FROM records WHERE namespace = $1 AND collection = $2 AND id = $3",
        RecordRow,
        fetch_optional,
        namespace.as_str(),
        name,
        id,
    )?
    .ok_or(Error::NotFound)?;
    decode(row, schema)
}

pub(super) async fn create(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    actor: &Actor,
    name: &str,
    id: &str,
    fields: &Fields,
) -> Result<Record, Error> {
    scope(namespace, definition, Some(actor))?;
    identifier(id)?;
    let fields = encode(fields, collection(definition, name)?)?;
    let sql = match native {
        Native::Sqlite(_) => {
            "INSERT INTO records (namespace, collection, id, fields) VALUES ($1, $2, $3, $4)"
        }
        Native::Postgres(_) => {
            "INSERT INTO records (namespace, collection, id, fields) VALUES ($1, $2, $3, CAST($4 AS JSONB))"
        }
    };
    execute!(native, sql, namespace.as_str(), name, id, &fields)?;
    let row = run!(
        native,
        "UPDATE records SET created_by = $4, last_edited_by = $4 \
         WHERE namespace = $1 AND collection = $2 AND id = $3 \
         RETURNING id, CAST(fields AS TEXT), created_by, last_edited_by",
        RecordRow,
        fetch_one,
        namespace.as_str(),
        name,
        id,
        actor.as_str(),
    )?;
    decode(row, collection(definition, name)?)
}

pub(super) async fn update(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    actor: &Actor,
    name: &str,
    id: &str,
    fields: &Fields,
) -> Result<Record, Error> {
    scope(namespace, definition, Some(actor))?;
    identifier(id)?;
    let schema = collection(definition, name)?;
    let fields = encode(fields, schema)?;
    let sql = match native {
        Native::Sqlite(_) => {
            "UPDATE records SET fields = $4 WHERE namespace = $1 AND collection = $2 AND id = $3"
        }
        Native::Postgres(_) => {
            "UPDATE records SET fields = CAST($4 AS JSONB) WHERE namespace = $1 AND collection = $2 AND id = $3"
        }
    };
    if execute!(native, sql, namespace.as_str(), name, id, &fields)? == 0 {
        return Err(Error::NotFound);
    }
    let row = run!(
        native,
        "UPDATE records SET last_edited_by = $4 \
         WHERE namespace = $1 AND collection = $2 AND id = $3 \
         RETURNING id, CAST(fields AS TEXT), created_by, last_edited_by",
        RecordRow,
        fetch_one,
        namespace.as_str(),
        name,
        id,
        actor.as_str(),
    )?;
    decode(row, schema)
}

pub(super) async fn delete(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    actor: &Actor,
    name: &str,
    id: &str,
) -> Result<(), Error> {
    scope(namespace, definition, Some(actor))?;
    collection(definition, name)?;
    identifier(id)?;
    affected(execute!(
        native,
        "DELETE FROM records WHERE namespace = $1 AND collection = $2 AND id = $3",
        namespace.as_str(),
        name,
        id,
    )?)
}

fn affected(count: u64) -> Result<(), Error> {
    if count == 0 {
        Err(Error::NotFound)
    } else {
        Ok(())
    }
}

pub(super) async fn find(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    name: &str,
    options: &Query,
) -> Result<Page<Record>, Error> {
    scope(namespace, definition, None)?;
    let schema = collection(definition, name)?;
    let mut statement = query::find(namespace.as_str(), name, schema, options, None)?;
    let cursor = Cursor::new(&(
        "records-v1",
        namespace.as_str(),
        name,
        schema,
        &options.filters,
        &options.sort,
    ))?;
    let last = cursor.decode(options.cursor.as_deref())?;
    if last.is_some() {
        statement = query::find(namespace.as_str(), name, schema, options, last.as_ref())?;
    }
    let mut rows = statement.fetch::<FindRow>(native).await?;
    let more = rows.len() > options.limit;
    rows.truncate(options.limit);
    let next = if more {
        let (id, _, _, _, text, integer, number, boolean, rank) =
            rows.last().ok_or(Error::Unavailable)?;
        let value = match rank {
            0 | 1 => None,
            2 => {
                let field = options.sort.as_ref().ok_or(Error::Invalid)?;
                let field = schema.fields.get(&field.field).ok_or(Error::Invalid)?;
                let value = match field.kind {
                    FieldType::String => text.clone(),
                    FieldType::Integer => integer.map(|v| v.to_string()),
                    FieldType::Number => number.map(|v| v.to_string()),
                    FieldType::Boolean => boolean.map(|v| v.to_string()),
                    FieldType::Json => None,
                };
                Some(value.ok_or(Error::Invalid)?)
            }
            _ => return Err(Error::Invalid),
        };
        Some(cursor.encode(Position {
            id: id.clone(),
            value,
            rank: *rank,
        })?)
    } else {
        None
    };
    let items: Vec<_> = rows
        .into_iter()
        .map(|(id, fields, creator, editor, ..)| decode((id, fields, creator, editor), schema))
        .collect::<Result<_, _>>()?;
    Ok(Page {
        items,
        cursor: next,
    })
}

fn cardinality(rule: &Relation) -> &'static str {
    match rule.cardinality {
        Cardinality::OneToOne => "one-to-one",
        Cardinality::OneToMany => "one-to-many",
        Cardinality::ManyToMany => "many-to-many",
    }
}

async fn rule_exists(
    native: &mut Native,
    namespace: &Namespace,
    name: &str,
    rule: &Relation,
) -> Result<bool, Error> {
    let existing = run!(
        native,
        "SELECT from_collection, to_collection, cardinality FROM relation_rules \
                 WHERE namespace = $1 AND relation = $2",
        (String, String, String),
        fetch_optional,
        namespace.as_str(),
        name,
    )?;
    match existing {
        Some((from, to, cardinality_name)) => {
            if from != rule.from || to != rule.to || cardinality_name != cardinality(rule) {
                return Err(Error::Conflict);
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

pub(super) async fn link(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    actor: &Actor,
    name: &str,
    from_id: &str,
    to_id: &str,
) -> Result<Link, Error> {
    scope(namespace, definition, Some(actor))?;
    identifier(from_id)?;
    identifier(to_id)?;
    let rule = relation(definition, name)?;
    if !rule_exists(native, namespace, name, rule).await? {
        execute!(
            native,
            "INSERT INTO relation_rules (namespace, relation, from_collection, to_collection, cardinality) \
             VALUES ($1, $2, $3, $4, $5)",
            namespace.as_str(),
            name,
            &rule.from,
            &rule.to,
            cardinality(rule),
        )?;
    }
    execute!(
        native,
        "INSERT INTO links (namespace, relation, from_id, to_id, from_collection, to_collection, cardinality) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        namespace.as_str(),
        name,
        from_id,
        to_id,
        &rule.from,
        &rule.to,
        cardinality(rule),
    )?;
    Ok(link_row(run!(
        native,
        "UPDATE links SET created_by = $5, last_edited_by = $5 \
         WHERE namespace = $1 AND relation = $2 AND from_id = $3 AND to_id = $4 \
         RETURNING from_id, to_id, created_by, last_edited_by",
        LinkRow,
        fetch_one,
        namespace.as_str(),
        name,
        from_id,
        to_id,
        actor.as_str(),
    )?))
}

pub(super) async fn unlink(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    actor: &Actor,
    name: &str,
    from_id: &str,
    to_id: &str,
) -> Result<(), Error> {
    scope(namespace, definition, Some(actor))?;
    identifier(from_id)?;
    identifier(to_id)?;
    if !rule_exists(native, namespace, name, relation(definition, name)?).await? {
        return Err(Error::NotFound);
    }
    affected(execute!(
        native,
        "DELETE FROM links WHERE namespace = $1 AND relation = $2 AND from_id = $3 AND to_id = $4",
        namespace.as_str(),
        name,
        from_id,
        to_id,
    )?)
}

pub(super) async fn traverse(
    native: &mut Native,
    namespace: &Namespace,
    definition: &Definition,
    name: &str,
    id: &str,
    options: &Traversal,
) -> Result<Page<LinkedRecord>, Error> {
    scope(namespace, definition, None)?;
    identifier(id)?;
    query::limit(options.limit)?;
    let rule = relation(definition, name)?;
    let (anchor, target) = match options.direction {
        Direction::Outgoing => (&rule.from, &rule.to),
        Direction::Incoming => (&rule.to, &rule.from),
    };
    get(native, namespace, definition, anchor, id).await?;
    let schema = collection(definition, target)?;
    let cursor = Cursor::new(&(
        "links-v1",
        namespace.as_str(),
        name,
        rule,
        id,
        options.direction,
        schema,
    ))?;
    let last = cursor.decode(options.cursor.as_deref())?;
    let mut rows = if rule_exists(native, namespace, name, rule).await? {
        query::traverse(namespace.as_str(), name, id, options, last.as_ref())?
            .fetch::<LinkedRow>(native)
            .await?
    } else {
        Vec::new()
    };
    let more = rows.len() > options.limit;
    rows.truncate(options.limit);
    let items: Vec<_> = rows
        .into_iter()
        .map(
            |(id, fields, creator, editor, from, to, link_creator, link_editor)| {
                Ok(LinkedRecord {
                    record: decode((id, fields, creator, editor), schema)?,
                    link: link_row((from, to, link_creator, link_editor)),
                })
            },
        )
        .collect::<Result<_, Error>>()?;
    let next = if more {
        let last = items.last().ok_or(Error::Unavailable)?;
        Some(cursor.encode(Position {
            id: last.record.id.clone(),
            value: None,
            rank: 0,
        })?)
    } else {
        None
    };
    Ok(Page {
        items,
        cursor: next,
    })
}
