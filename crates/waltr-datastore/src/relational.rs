//! Native relational storage. The caller supplies the current authorized namespace and schema.

mod definitions;
mod operations;
mod query;

use std::{path::Path, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{
    PgPool, Postgres, Sqlite, SqlitePool,
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::{Actor, Attribution, Definition, Error, Namespace};

pub use query::{Direction, Filter, FilterOp, Query, Sort, Traversal};

/// A record's declared fields. An absent key is distinct from JSON null.
pub type Fields = Map<String, Value>;

/// A record in the requested collection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    /// Stable identifier within the collection.
    pub id: String,
    /// Current record fields.
    pub fields: Fields,
    /// Current authorship facts.
    pub attribution: Attribution,
}

/// A directed edge in the requested relation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link {
    /// Source record identifier.
    pub from_id: String,
    /// Target record identifier.
    pub to_id: String,
    /// Current authorship facts.
    pub attribution: Attribution,
}

/// A neighbor and the edge that connects it to the traversal start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkedRecord {
    /// The neighbor record in the selected direction.
    pub record: Record,
    /// The edge in canonical source-to-target order.
    pub link: Link,
}

/// One bounded page of a query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    /// Results in the requested order.
    pub items: Vec<T>,
    /// Opaque continuation for the same query, if more results exist.
    pub cursor: Option<String>,
}

/// A migrated native relational database with an owned connection pool.
///
/// For registered definitions, a native link insert derives its internal rule from the catalog
/// contract and does not require a prior Waltr link.
#[derive(Clone)]
pub struct Relational(Pool);

impl std::fmt::Debug for Relational {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relational").finish_non_exhaustive()
    }
}

#[derive(Clone)]
enum Pool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

enum Native {
    Sqlite(sqlx::Transaction<'static, Sqlite>),
    Postgres(sqlx::Transaction<'static, Postgres>),
}

/// An owned, serializable transaction. Dropping it before commit rolls back.
pub struct Transaction {
    native: Native,
    rollback_only: bool,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("rollback_only", &self.rollback_only)
            .finish_non_exhaustive()
    }
}

macro_rules! run {
    ($native:expr, $sql:expr, $result:ty, $action:ident $(, $arg:expr)* $(,)?) => {{
        match $native {
            Native::Sqlite(tx) => sqlx::query_as::<_, $result>($sql)
                $(.bind($arg))* .$action(&mut **tx).await,
            Native::Postgres(tx) => sqlx::query_as::<_, $result>($sql)
                $(.bind($arg))* .$action(&mut **tx).await,
        }.map_err(sql_error)
    }};
}

macro_rules! execute {
    ($native:expr, $sql:expr $(, $arg:expr)* $(,)?) => {{
        match $native {
            Native::Sqlite(tx) => sqlx::query($sql)
                $(.bind($arg))* .execute(&mut **tx).await.map(|r| r.rows_affected()),
            Native::Postgres(tx) => sqlx::query($sql)
                $(.bind($arg))* .execute(&mut **tx).await.map(|r| r.rows_affected()),
        }.map_err(sql_error)
    }};
}

use execute;
use run;

fn sql_error(error: sqlx::Error) -> Error {
    if let sqlx::Error::Database(database) = &error {
        if database.is_unique_violation() || database.is_foreign_key_violation() {
            return Error::Conflict;
        }
        if let Some(code) = database.code() {
            match code.as_ref() {
                "40001" | "40P01" | "55P03" | "23514" | "P0001" => return Error::Conflict,
                code if code
                    .parse::<i32>()
                    .is_ok_and(|code| matches!(code & 255, 5 | 6 | 19)) =>
                {
                    return Error::Conflict;
                }
                _ => {}
            }
        }
    }
    Error::Unavailable
}

fn commit_error(error: sqlx::Error) -> Error {
    if sql_error(error) == Error::Conflict {
        Error::Conflict
    } else {
        Error::Uncertain
    }
}

const CONNECTIONS: u32 = 8;
const WAIT: Duration = Duration::from_secs(5);

impl Relational {
    /// Open and migrate a local SQLite database in WAL mode.
    pub async fn open_sqlite(path: &Path) -> Result<Self, Error> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(WAIT);
        let pool = SqlitePoolOptions::new()
            .max_connections(CONNECTIONS)
            .acquire_timeout(WAIT)
            .connect_with(options)
            .await
            .map_err(sql_error)?;
        sqlx::migrate!("./migrations/sqlite")
            .run(&pool)
            .await
            .map_err(|_| Error::Unavailable)?;
        Ok(Self(Pool::Sqlite(pool)))
    }

    /// Open PostgreSQL and migrate the dedicated datastore schema.
    pub async fn open_postgres(url: &str) -> Result<Self, Error> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|_| Error::Invalid)?
            .options([("search_path", "waltr_datastore,pg_catalog,pg_temp")]);
        let pool = PgPoolOptions::new()
            .max_connections(CONNECTIONS)
            .acquire_timeout(WAIT)
            .connect_with(options)
            .await
            .map_err(sql_error)?;
        let mut setup = pool.begin().await.map_err(sql_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(417989528471)")
            .execute(&mut *setup)
            .await
            .map_err(sql_error)?;
        sqlx::query("CREATE SCHEMA IF NOT EXISTS waltr_datastore")
            .execute(&mut *setup)
            .await
            .map_err(sql_error)?;
        setup.commit().await.map_err(sql_error)?;
        sqlx::migrate!("./migrations/postgres")
            .run(&pool)
            .await
            .map_err(|_| Error::Unavailable)?;
        Ok(Self(Pool::Postgres(pool)))
    }

    /// Start an owned native serializable transaction.
    pub async fn begin(&self) -> Result<Transaction, Error> {
        let native = match &self.0 {
            Pool::Sqlite(pool) => Native::Sqlite(pool.begin().await.map_err(sql_error)?),
            Pool::Postgres(pool) => {
                let mut tx = pool
                    .begin_with("BEGIN ISOLATION LEVEL SERIALIZABLE")
                    .await
                    .map_err(sql_error)?;
                sqlx::query("SET LOCAL synchronous_commit = on")
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
                set_statement_timeout(&mut tx).await?;
                Native::Postgres(tx)
            }
        };
        Ok(Transaction {
            native,
            rollback_only: false,
        })
    }

    /// The lock waits for a potentially disconnected committer before testing receipt presence.
    pub async fn has_receipt(&self, invocation: &str) -> Result<bool, Error> {
        validate_invocation(invocation)?;
        let mut native = match &self.0 {
            Pool::Sqlite(pool) => Native::Sqlite(
                pool.begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(sql_error)?,
            ),
            Pool::Postgres(pool) => {
                let mut tx = pool
                    .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
                    .await
                    .map_err(sql_error)?;
                set_statement_timeout(&mut tx).await?;
                Native::Postgres(tx)
            }
        };
        lock_receipt(&mut native, invocation).await?;
        let found = run!(
            &mut native,
            "SELECT invocation FROM commit_receipts WHERE invocation = $1",
            (String,),
            fetch_optional,
            invocation,
        )?
        .is_some();
        Transaction {
            native,
            rollback_only: false,
        }
        .rollback()
        .await?;
        Ok(found)
    }
}

async fn set_statement_timeout(tx: &mut sqlx::Transaction<'_, Postgres>) -> Result<(), Error> {
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut **tx)
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn validate_invocation(invocation: &str) -> Result<(), Error> {
    if invocation.is_empty() || invocation.len() > 512 || invocation.chars().any(char::is_control) {
        return Err(Error::Invalid);
    }
    Ok(())
}

async fn lock_receipt(native: &mut Native, invocation: &str) -> Result<(), Error> {
    if let Native::Postgres(tx) = native {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 417989528471))")
            .bind(invocation)
            .execute(&mut **tx)
            .await
            .map_err(sql_error)?;
    }
    Ok(())
}

impl Transaction {
    /// Commit all changes and a receipt for a trusted invocation key atomically.
    pub async fn commit(mut self, invocation: &str) -> Result<(), Error> {
        validate_invocation(invocation)?;
        if self.rollback_only {
            self.rollback().await?;
            return Err(Error::Conflict);
        }
        lock_receipt(&mut self.native, invocation).await?;
        execute!(
            &mut self.native,
            "INSERT INTO commit_receipts (invocation) VALUES ($1)",
            invocation,
        )?;
        self.finish().await
    }

    /// Discard all changes in the transaction.
    pub async fn rollback(self) -> Result<(), Error> {
        match self.native {
            Native::Sqlite(tx) => tx.rollback().await,
            Native::Postgres(tx) => tx.rollback().await,
        }
        .map_err(sql_error)
    }

    /// Mark a transaction rollback-only after a mutation was rejected outside the adapter.
    pub fn mark_rollback_only(&mut self) {
        self.rollback_only = true;
    }

    async fn finish(self) -> Result<(), Error> {
        match self.native {
            Native::Sqlite(tx) => tx.commit().await,
            Native::Postgres(tx) => tx.commit().await,
        }
        .map_err(commit_error)
    }
}

macro_rules! reads {
    ($($(#[$meta:meta])* $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)+) => {
        impl Relational {
            $($(#[$meta])* pub async fn $method(&self, namespace: &Namespace, definition: &Definition, $($arg: $ty),*)
                -> Result<$result, Error> {
                let mut tx = self.begin().await?;
                let result = tx.$method(namespace, definition, $($arg),*).await?;
                tx.rollback().await?;
                Ok(result)
            })+
        }
        impl Transaction {
            $($(#[$meta])* pub async fn $method(&mut self, namespace: &Namespace, definition: &Definition, $($arg: $ty),*)
                -> Result<$result, Error> {
                operations::$method(&mut self.native, namespace, definition, $($arg),*).await
            })+
        }
    }
}

macro_rules! writes {
    ($($(#[$meta:meta])* $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)+) => {
        impl Relational {
            $($(#[$meta])* pub async fn $method(&self, namespace: &Namespace, definition: &Definition, actor: &Actor, $($arg: $ty),*)
                -> Result<$result, Error> {
                let mut tx = self.begin().await?;
                let result = tx.$method(namespace, definition, actor, $($arg),*).await?;
                tx.finish().await?;
                Ok(result)
            })+
        }
        impl Transaction {
            $($(#[$meta])* pub async fn $method(&mut self, namespace: &Namespace, definition: &Definition, actor: &Actor, $($arg: $ty),*)
                -> Result<$result, Error> {
                if self.rollback_only {
                    return Err(Error::Conflict);
                }
                self.rollback_only = true;
                let result = operations::$method(&mut self.native, namespace, definition, actor, $($arg),*).await;
                self.rollback_only = result.is_err();
                result
            })+
        }
    }
}

reads! {
    /// Fetch one record by identifier.
    get(collection: &str, id: &str) -> Record;
    /// Fetch a bounded page of records that match the query.
    find(collection: &str, query: &Query) -> Page<Record>;
    /// Fetch a bounded page of neighbors in one declared relation.
    traverse(relation: &str, id: &str, query: &Traversal) -> Page<LinkedRecord>;
}

writes! {
    /// Create a record. Reject a duplicate identifier in the same collection and namespace.
    create(collection: &str, id: &str, fields: &Fields) -> Record;
    /// Replace a record's fields and preserve creator attribution.
    update(collection: &str, id: &str, fields: &Fields) -> Record;
    /// Delete a record and its incident edges.
    delete(collection: &str, id: &str) -> ();
    /// Create a directed edge between two records in the same namespace.
    link(relation: &str, from_id: &str, to_id: &str) -> Link;
    /// Delete a directed edge without deleting either endpoint.
    unlink(relation: &str, from_id: &str, to_id: &str) -> ();
}
