use std::fmt::Write;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, postgres::PgRow, sqlite::SqliteRow};

use super::{Native, sql_error};
use crate::{Collection, Error, FieldType, MAX_PAGE};

const MAX_FILTERS: usize = 8;
const MAX_CURSOR: usize = 8192;
pub(super) const MAX_TEXT_BYTES: usize = 1024;

fn default_limit() -> usize {
    50
}

/// A bounded collection query.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    /// A conjunction of typed field predicates.
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Optional indexed field order. The default order uses record IDs.
    #[serde(default)]
    pub sort: Option<Sort>,
    /// Maximum number of results on this page.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Opaque continuation from the same query.
    #[serde(default)]
    pub cursor: Option<String>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            sort: None,
            limit: default_limit(),
            cursor: None,
        }
    }
}

/// A predicate on a declared collection field.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// The declared field name.
    pub field: String,
    /// The operation that tests the field.
    pub op: FilterOp,
    /// Scalar comparison input. Omit it for null and absence tests.
    #[serde(default)]
    pub value: Option<Value>,
}

/// Supported filter operations.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilterOp {
    /// Equal to a scalar.
    Eq,
    /// Less than a scalar.
    Lt,
    /// Less than or equal to a scalar.
    Lte,
    /// Greater than a scalar.
    Gt,
    /// Greater than or equal to a scalar.
    Gte,
    /// The field contains an explicit JSON null.
    IsNull,
    /// The field is absent.
    IsMissing,
}

/// Keyset order by an indexed scalar field with record ID as a tie-breaker.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    /// The declared indexed field name.
    pub field: String,
    /// Reverse the full order, which includes the ID tie-breaker.
    #[serde(default)]
    pub descending: bool,
}

/// Direction of a single edge traversal.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Follow links from the source record to targets.
    #[default]
    Outgoing,
    /// Follow links from the target record to sources.
    Incoming,
}

/// A bounded one-hop traversal query.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Traversal {
    /// Direction relative to the start record.
    #[serde(default)]
    pub direction: Direction,
    /// Maximum number of neighbors on this page.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Opaque continuation from the same traversal.
    #[serde(default)]
    pub cursor: Option<String>,
}

impl Default for Traversal {
    fn default() -> Self {
        Self {
            direction: Direction::default(),
            limit: default_limit(),
            cursor: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Position {
    pub(super) id: String,
    pub(super) value: Option<String>,
    pub(super) rank: i64,
}

pub(super) struct Cursor([u8; 32]);

impl Cursor {
    pub(super) fn new(scope: &impl Serialize) -> Result<Self, Error> {
        Ok(Self(
            Sha256::digest(serde_json::to_vec(scope).map_err(|_| Error::Invalid)?).into(),
        ))
    }

    pub(super) fn encode(&self, position: Position) -> Result<String, Error> {
        let body = serde_json::to_vec(&position).map_err(|_| Error::Invalid)?;
        let mut bytes = Vec::with_capacity(body.len() + self.0.len());
        bytes.extend(self.0);
        bytes.extend(body);
        let cursor = URL_SAFE_NO_PAD.encode(bytes);
        if cursor.len() > MAX_CURSOR {
            return Err(Error::Limit);
        }
        Ok(cursor)
    }

    pub(super) fn decode(&self, cursor: Option<&str>) -> Result<Option<Position>, Error> {
        let Some(cursor) = cursor else {
            return Ok(None);
        };
        if cursor.len() > MAX_CURSOR {
            return Err(Error::Limit);
        }
        let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| Error::Invalid)?;
        let body = bytes
            .strip_prefix(self.0.as_slice())
            .ok_or(Error::Invalid)?;
        let position: Position = serde_json::from_slice(body).map_err(|_| Error::Invalid)?;
        crate::identifier(&position.id)?;
        Ok(Some(position))
    }
}

pub(super) fn limit(limit: usize) -> Result<i64, Error> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(Error::Limit);
    }
    i64::try_from(limit + 1).map_err(|_| Error::Limit)
}

#[derive(Clone)]
pub(super) enum Bind {
    Text(String),
    Integer(i64),
    Number(f64),
}

impl From<&str> for Bind {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<i64> for Bind {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

#[derive(Default)]
pub(super) struct Statement {
    pub(super) sql: String,
    binds: Vec<Bind>,
}

impl Statement {
    pub(super) fn bind(&mut self, value: impl Into<Bind>) -> String {
        self.binds.push(value.into());
        format!("${}", self.binds.len())
    }

    pub(super) async fn fetch<T>(&self, native: &mut Native) -> Result<Vec<T>, Error>
    where
        T: for<'r> FromRow<'r, PgRow> + for<'r> FromRow<'r, SqliteRow> + Send + Unpin,
    {
        macro_rules! bind_query {
            ($tx:expr) => {{
                let mut query = sqlx::query_as::<_, T>(&self.sql);
                for value in &self.binds {
                    query = match value {
                        Bind::Text(value) => query.bind(value),
                        Bind::Integer(value) => query.bind(*value),
                        Bind::Number(value) => query.bind(*value),
                    };
                }
                query.fetch_all(&mut **$tx).await
            }};
        }
        match native {
            Native::Sqlite(tx) => bind_query!(tx),
            Native::Postgres(tx) => bind_query!(tx),
        }
        .map_err(sql_error)
    }
}

fn storage_column(kind: FieldType) -> Result<&'static str, Error> {
    match kind {
        FieldType::String => Ok("text_value"),
        FieldType::Integer => Ok("integer_value"),
        FieldType::Number => Ok("number_value"),
        FieldType::Boolean => Ok("boolean_value"),
        FieldType::Json => Err(Error::Invalid),
    }
}

fn scalar(kind: FieldType, value: &Value) -> Result<Bind, Error> {
    match kind {
        FieldType::Boolean => value.as_bool().map(|v| Bind::Integer(i64::from(v))),
        FieldType::Integer => value.as_i64().map(Bind::Integer),
        FieldType::Number => value.as_f64().filter(|v| v.is_finite()).map(Bind::Number),
        FieldType::String => value
            .as_str()
            .filter(|s| s.len() <= MAX_TEXT_BYTES && !s.contains('\0'))
            .map(Bind::from),
        FieldType::Json => None,
    }
    .ok_or(Error::Invalid)
}

fn sort_key(kind: FieldType, value: &str) -> Result<Bind, Error> {
    match kind {
        FieldType::String if value.len() <= MAX_TEXT_BYTES && !value.contains('\0') => {
            Some(Bind::from(value))
        }
        FieldType::Integer => value.parse().ok().map(Bind::Integer),
        FieldType::Boolean if matches!(value, "0" | "1") => value.parse().ok().map(Bind::Integer),
        FieldType::Number => value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Bind::Number),
        _ => None,
    }
    .ok_or(Error::Invalid)
}

pub(super) fn find(
    namespace: &str,
    collection: &str,
    schema: &Collection,
    query: &Query,
    last: Option<&Position>,
) -> Result<Statement, Error> {
    if query.filters.len() > MAX_FILTERS {
        return Err(Error::Limit);
    }
    let page = limit(query.limit)?;
    let mut statement = Statement::default();
    let partition = statement.bind(namespace);
    let collection = statement.bind(collection);
    statement
        .sql
        .push_str("SELECT r.id, CAST(r.fields AS TEXT), r.created_by, r.last_edited_by");

    let sort = if let Some(sort) = &query.sort {
        let spec = schema.fields.get(&sort.field).ok_or(Error::Invalid)?;
        if !spec.indexed {
            return Err(Error::Invalid);
        }
        let column = storage_column(spec.kind)?;
        let field = statement.bind(sort.field.as_str());
        let rank = format!(
            "(CASE WHEN s.field IS NULL THEN 0 WHEN s.kind = 'null' THEN 1 \
             WHEN s.{column} IS NOT NULL THEN 2 ELSE 3 END)"
        );
        write!(
            statement.sql,
            ", s.text_value, s.integer_value, s.number_value, s.boolean_value, CAST({rank} AS BIGINT) \
             FROM records r LEFT JOIN field_indexes s ON \
             s.namespace = r.namespace AND s.collection = r.collection AND s.id = r.id AND s.field = {field}",
        )
        .expect("formatting SQL");
        Some((column, spec.kind, sort.descending, rank))
    } else {
        statement.sql.push_str(
            ", CAST(NULL AS TEXT), CAST(NULL AS BIGINT), CAST(NULL AS DOUBLE PRECISION), \
             CAST(NULL AS BIGINT), CAST(0 AS BIGINT) FROM records r",
        );
        None
    };
    write!(
        statement.sql,
        " WHERE r.namespace = {partition} AND r.collection = {collection}",
    )
    .expect("formatting SQL");

    for filter in &query.filters {
        let spec = schema.fields.get(&filter.field).ok_or(Error::Invalid)?;
        let field = statement.bind(filter.field.as_str());
        let skip = match filter.op {
            FilterOp::IsNull | FilterOp::IsMissing => {
                if filter.value.is_some() {
                    return Err(Error::Invalid);
                }
                match filter.op {
                    FilterOp::IsMissing => ("NOT IN", String::new()),
                    _ => ("IN", " AND f.kind = 'null'".to_owned()),
                }
            }
            op => {
                if !matches!(op, FilterOp::Eq) && matches!(spec.kind, FieldType::Boolean) {
                    return Err(Error::Invalid);
                }
                let value = scalar(spec.kind, filter.value.as_ref().ok_or(Error::Invalid)?)?;
                let column = storage_column(spec.kind)?;
                let arg = statement.bind(value);
                let op = match op {
                    FilterOp::Eq => "=",
                    FilterOp::Lt => "<",
                    FilterOp::Lte => "<=",
                    FilterOp::Gt => ">",
                    FilterOp::Gte => ">=",
                    FilterOp::IsNull | FilterOp::IsMissing => unreachable!(),
                };
                ("IN", format!(" AND f.{column} {op} {arg}"))
            }
        };
        let (membership, predicate) = skip;
        write!(
            statement.sql,
            " AND r.id {membership} (SELECT f.id FROM field_indexes f \
             WHERE f.namespace = {partition} AND f.collection = {collection} AND f.field = {field}{predicate})"
        )
        .expect("formatting SQL");
    }

    if let Some((column, kind, descending, rank)) = sort {
        let (cmp, order) = if descending {
            (">", "DESC")
        } else {
            ("<", "ASC")
        };
        // Compare the previous tuple against each candidate in the selected direction.
        if let Some(last) = last {
            let r = statement.bind(last.rank);
            let id = statement.bind(last.id.as_str());
            let within = match (last.rank, &last.value) {
                (0 | 1, None) => format!("{id} {cmp} r.id"),
                (2, Some(value)) => {
                    let v = statement.bind(sort_key(kind, value)?);
                    format!("{v} {cmp} s.{column} OR ({v} = s.{column} AND {id} {cmp} r.id)")
                }
                _ => return Err(Error::Invalid),
            };
            write!(
                statement.sql,
                " AND ({r} {cmp} {rank} OR ({r} = {rank} AND ({within})))"
            )
            .expect("formatting SQL");
        }
        write!(
            statement.sql,
            " ORDER BY {rank} {order}, s.{column} {order}, r.id {order}"
        )
        .expect("formatting SQL");
    } else {
        if let Some(last) = last {
            if last.rank != 0 || last.value.is_some() {
                return Err(Error::Invalid);
            }
            let id = statement.bind(last.id.as_str());
            write!(statement.sql, " AND r.id > {id}").expect("formatting SQL");
        }
        statement.sql.push_str(" ORDER BY r.id");
    }
    let page = statement.bind(page);
    write!(statement.sql, " LIMIT {page}").expect("formatting SQL");
    Ok(statement)
}

pub(super) fn traverse(
    namespace: &str,
    relation: &str,
    anchor: &str,
    query: &Traversal,
    last: Option<&Position>,
) -> Result<Statement, Error> {
    let (here, there) = match query.direction {
        Direction::Outgoing => ("from", "to"),
        Direction::Incoming => ("to", "from"),
    };
    let mut statement = Statement::default();
    let partition = statement.bind(namespace);
    let relation = statement.bind(relation);
    let anchor = statement.bind(anchor);
    statement.sql = format!(
        "SELECT r.id, CAST(r.fields AS TEXT), r.created_by, r.last_edited_by, \
         l.from_id, l.to_id, l.created_by, l.last_edited_by FROM links l \
         JOIN records r ON r.namespace = l.namespace AND r.collection = l.{there}_collection AND r.id = l.{there}_id \
         WHERE l.namespace = {partition} AND l.relation = {relation} AND l.{here}_id = {anchor}"
    );
    if let Some(last) = last {
        if last.rank != 0 || last.value.is_some() {
            return Err(Error::Invalid);
        }
        let id = statement.bind(last.id.as_str());
        write!(statement.sql, " AND l.{there}_id > {id}").expect("formatting SQL");
    }
    let page = statement.bind(limit(query.limit)?);
    write!(statement.sql, " ORDER BY l.{there}_id LIMIT {page}").expect("formatting SQL");
    Ok(statement)
}
