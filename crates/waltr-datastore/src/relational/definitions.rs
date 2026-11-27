use std::collections::BTreeMap;

use super::{Native, Pool, Relational, Transaction, set_statement_timeout, sql_error};
use crate::{Definition, Error, identifier};

impl Relational {
    /// Check a catalog candidate against durable definitions without registering new names.
    pub async fn check_definitions<'a>(
        &self,
        definitions: impl IntoIterator<Item = (&'a str, &'a Definition)>,
    ) -> Result<(), Error> {
        let definitions = validated(definitions)?;
        if definitions.is_empty() {
            return Ok(());
        }
        let mut tx = self.begin().await?;
        for (name, definition) in definitions {
            if let Some(stored) = load(&mut tx.native, name).await? {
                check(definition, &stored)?;
            }
        }
        tx.rollback().await
    }

    /// Verify and register the full candidate in one database transaction.
    ///
    /// Definitions use logical names, independent of backend partition and current activation.
    /// This also records object definitions so a name cannot change type or scope after removal.
    /// Omitted names retain their contracts. Pass the final candidate before publication.
    pub async fn ensure_definitions<'a>(
        &self,
        definitions: impl IntoIterator<Item = (&'a str, &'a Definition)>,
    ) -> Result<(), Error> {
        let definitions = validated(definitions)?;
        if definitions.is_empty() {
            return Ok(());
        }
        let sqlite = matches!(&self.0, Pool::Sqlite(_));
        let native = match &self.0 {
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
                sqlx::query("SET LOCAL synchronous_commit = on")
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
                Native::Postgres(tx)
            }
        };
        let mut tx = Transaction {
            native,
            rollback_only: false,
        };
        for (name, definition) in definitions {
            if let Some(stored) = load(&mut tx.native, name).await? {
                check(definition, &stored)?;
                continue;
            }
            let encoded = serde_json::to_string(definition).map_err(|_| Error::Invalid)?;
            let sql = if sqlite {
                "INSERT INTO definition_contracts (name, definition) VALUES ($1, $2)"
            } else {
                "INSERT INTO definition_contracts (name, definition) VALUES ($1, CAST($2 AS JSONB))
                    ON CONFLICT (name) DO NOTHING"
            };
            if super::execute!(&mut tx.native, sql, name, encoded)? == 0 {
                // At READ COMMITTED this statement sees a concurrent insertion that won.
                let stored = load(&mut tx.native, name)
                    .await?
                    .ok_or(Error::Unavailable)?;
                check(definition, &stored)?;
            }
        }
        tx.finish().await
    }
}

fn validated<'a>(
    definitions: impl IntoIterator<Item = (&'a str, &'a Definition)>,
) -> Result<BTreeMap<&'a str, &'a Definition>, Error> {
    let mut result = BTreeMap::new();
    for (name, definition) in definitions {
        identifier(name)?;
        definition.validate()?;
        if result.insert(name, definition).is_some() {
            return Err(Error::Invalid);
        }
    }
    Ok(result)
}

async fn load(native: &mut Native, name: &str) -> Result<Option<String>, Error> {
    Ok(super::run!(
        native,
        "SELECT CAST(definition AS TEXT) FROM definition_contracts WHERE name = $1",
        (String,),
        fetch_optional,
        name,
    )?
    .map(|(value,)| value))
}

fn check(definition: &Definition, stored: &str) -> Result<(), Error> {
    let stored: Definition = serde_json::from_str(stored).map_err(|_| Error::Unavailable)?;
    if &stored == definition {
        Ok(())
    } else {
        Err(Error::Conflict)
    }
}
