//! SQL journal for non-replayable, authenticated sessionless effects.

use super::PersistencePool;
use crate::agent_host::external_journal::{
    ExternalDispatch, ExternalDispatchStore, JournalError, JournalFuture,
};

#[derive(Clone)]
/// Durable external dispatch ledger in the initialized Waltr database.
pub struct SqlExternalDispatchStore {
    pool: PersistencePool,
}

impl std::fmt::Debug for SqlExternalDispatchStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlExternalDispatchStore")
            .finish_non_exhaustive()
    }
}

impl SqlExternalDispatchStore {
    pub(super) fn new(pool: PersistencePool) -> Self {
        Self { pool }
    }
}

macro_rules! backend {
    ($pool:expr, |$p:ident, $s:ident| $body:block) => {
        match $pool {
            PersistencePool::Sqlite($p) => {
                let $s = "";
                $body
            }
            PersistencePool::PostgreSql($p) => {
                let $s = "waltr.";
                $body
            }
        }
    };
}

fn association(dispatch: &ExternalDispatch) -> Result<String, JournalError> {
    if dispatch.operation_id.is_empty()
        || dispatch.identity.is_empty()
        || dispatch.source_revision.is_empty()
        || dispatch.call.id != dispatch.operation_id
    {
        return Err(JournalError::Invalid);
    }
    serde_json::to_string(dispatch).map_err(|_| JournalError::Invalid)
}

impl ExternalDispatchStore for SqlExternalDispatchStore {
    fn associate(&self, dispatch: ExternalDispatch) -> JournalFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let association = association(&dispatch)?;
            let rows = backend!(&pool, |p, s| {
                sqlx::query(&format!("INSERT INTO {s}external_capability_dispatch (operation_id,association,state) VALUES ($1,$2,'uncertain') ON CONFLICT DO NOTHING"))
                    .bind(&dispatch.operation_id).bind(association).execute(p).await.map_err(|_| JournalError::Unavailable)?.rows_affected()
            });
            if rows == 1 {
                Ok(())
            } else {
                Err(JournalError::Conflict)
            }
        })
    }

    fn complete(
        &self,
        dispatch: ExternalDispatch,
        outcome: serde_json::Value,
    ) -> JournalFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let association = association(&dispatch)?;
            let outcome = serde_json::to_string(&outcome).map_err(|_| JournalError::Invalid)?;
            let rows = backend!(&pool, |p, s| {
                sqlx::query(&format!("UPDATE {s}external_capability_dispatch SET state='complete',outcome=$3 WHERE operation_id=$1 AND association=$2 AND state='uncertain' AND outcome IS NULL"))
                    .bind(&dispatch.operation_id).bind(association).bind(outcome).execute(p).await.map_err(|_| JournalError::Unavailable)?.rows_affected()
            });
            if rows == 1 {
                Ok(())
            } else {
                Err(JournalError::Conflict)
            }
        })
    }
}
