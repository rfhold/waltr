//! Waltr-owned SQL adapter for selected durable business text.
use super::{Persistence, PersistencePool};
use crate::routine::text_results::{
    InvocationTextScope, MAX_CHUNK_BYTES, MAX_ENCODED_BYTES, MAX_RESULTS, MAX_TEXT_BYTES,
    PUBLISH_CREDIT, TextChunk, TextFuture, TextResultDescriptor, TextResultError, TextResultStore,
    TextSource, bounded_identifier, reserve,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Write, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
/// Shares the initialized Waltr pool and a non-bypassable process-wide working budget.
pub struct SqlTextResultStore {
    pool: PersistencePool,
    objects: Arc<dyn crate::routine_objects::ImmutableObjects>,
}
impl std::fmt::Debug for SqlTextResultStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlTextResultStore")
            .field("backend", &self.pool.backend())
            .finish_non_exhaustive()
    }
}
impl Persistence {
    /// Shared text adapter; another store never creates another process-wide budget.
    pub fn script_text_result_store(&self) -> Arc<SqlTextResultStore> {
        Arc::new(SqlTextResultStore {
            pool: self.pool.clone(),
            objects: self.objects.clone(),
        })
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
fn sql_error(error: sqlx::Error) -> TextResultError {
    if error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
    {
        TextResultError::Conflict
    } else {
        TextResultError::Unavailable
    }
}
fn cancelled(token: &CancellationToken) -> Result<(), TextResultError> {
    if token.is_cancelled() {
        Err(TextResultError::Cancelled)
    } else {
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Deserialize)]
struct StoredOwner {
    caller: StoredCaller,
}
#[derive(Deserialize)]
struct StoredCaller {
    principal: String,
}
fn check_owner(record: &str, scope: &InvocationTextScope) -> Result<(), TextResultError> {
    if record.len() > 4096 {
        return Err(TextResultError::Unavailable);
    }
    let owner: StoredOwner =
        serde_json::from_str(record).map_err(|_| TextResultError::Unavailable)?;
    if owner.caller.principal == scope.principal() {
        Ok(())
    } else {
        Err(TextResultError::Denied)
    }
}
async fn authorize(
    pool: &PersistencePool,
    scope: &InvocationTextScope,
) -> Result<String, TextResultError> {
    backend!(pool, |pool, schema| {
        let row: Option<(String, String)> = sqlx::query_as(&format!(
            "SELECT record,status FROM {schema}script_invocations WHERE id=$1"
        ))
        .bind(scope.invocation_id())
        .fetch_optional(pool)
        .await
        .map_err(sql_error)?;
        let (record, status) = row.ok_or(TextResultError::Denied)?;
        check_owner(&record, scope)?;
        Ok(status)
    })
}

struct CountWriter(usize);
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_ENCODED_BYTES)
            .ok_or_else(|| std::io::Error::other("text envelope limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[derive(Serialize)]
struct CanonicalEnvelope<'a> {
    descriptor: &'a TextResultDescriptor,
    text: &'a str,
}
fn envelope_charge(
    descriptor: &mut TextResultDescriptor,
    text: &str,
) -> Result<(), TextResultError> {
    // The only self-reference is the decimal charge width. It converges in at most
    // three passes for this eight-digit maximum; no escaped text is allocated.
    for _ in 0..4 {
        let mut counter = CountWriter(0);
        serde_json::to_writer(&mut counter, &CanonicalEnvelope { descriptor, text })
            .map_err(|_| TextResultError::Quota)?;
        let actual = u64::try_from(counter.0).map_err(|_| TextResultError::Quota)?;
        if actual == descriptor.encoded_charge {
            return Ok(());
        }
        descriptor.encoded_charge = actual;
    }
    Err(TextResultError::Invalid)
}
fn decode_descriptor(record: &str) -> Result<TextResultDescriptor, TextResultError> {
    if record.len() > 1024 {
        return Err(TextResultError::Unavailable);
    }
    serde_json::from_str(record).map_err(|_| TextResultError::Unavailable)
}

impl TextResultStore for SqlTextResultStore {
    fn publish(
        &self,
        scope: InvocationTextScope,
        name: String,
        helper_id: Option<String>,
        mut source: Box<dyn TextSource>,
        cancellation: CancellationToken,
    ) -> TextFuture<TextResultDescriptor> {
        let pool = self.pool.clone();
        let objects = self.objects.clone();
        Box::pin(async move {
            if authorize(&pool, &scope).await? != "running" {
                return Err(TextResultError::Conflict);
            }
            if !bounded_identifier(&name, 64)
                || name.capacity() > 256
                || helper_id
                    .as_ref()
                    .is_some_and(|h| !bounded_identifier(h, 128) || h.capacity() > 512)
            {
                return Err(TextResultError::Invalid);
            }
            let _credit = reserve(PUBLISH_CREDIT, &cancellation).await?;
            let mut content = Vec::with_capacity(MAX_TEXT_BYTES);
            let mut chunk = vec![0; MAX_CHUNK_BYTES];
            loop {
                let count = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(TextResultError::Cancelled),
                    count = source.read(&mut chunk) => count.map_err(|error| match error {
                        TextResultError::Invalid | TextResultError::Quota | TextResultError::Cancelled => error,
                        _ => TextResultError::Unavailable,
                    })?,
                };
                if count > chunk.len() {
                    return Err(TextResultError::Invalid);
                }
                if count == 0 {
                    break;
                }
                let total = content
                    .len()
                    .checked_add(count)
                    .ok_or(TextResultError::Quota)?;
                if total > MAX_TEXT_BYTES {
                    return Err(TextResultError::Quota);
                }
                content.extend_from_slice(&chunk[..count]);
            }
            drop(source);
            drop(chunk);
            cancelled(&cancellation)?;
            let text = std::str::from_utf8(&content).map_err(|_| TextResultError::Invalid)?;
            let mut descriptor = TextResultDescriptor {
                version: 1,
                id: uuid::Uuid::new_v4().to_string(),
                name,
                sha256: sha256_hex(&content),
                utf8_bytes: u64::try_from(content.len()).map_err(|_| TextResultError::Quota)?,
                encoded_charge: 0,
                helper_id,
            };
            envelope_charge(&mut descriptor, text)?;
            let encoded =
                serde_json::to_string(&descriptor).map_err(|_| TextResultError::Invalid)?;
            if encoded.len() > 1024 {
                return Err(TextResultError::Invalid);
            }
            let charge =
                i64::try_from(descriptor.encoded_charge).map_err(|_| TextResultError::Quota)?;
            let remaining = i64::try_from(MAX_ENCODED_BYTES)
                .map_err(|_| TextResultError::Quota)?
                .checked_sub(charge)
                .ok_or(TextResultError::Quota)?;
            cancelled(&cancellation)?;
            let object = crate::routine_objects::ObjectRef::for_bytes(
                crate::routine_objects::Namespace::Text,
                &content,
            )
            .map_err(|_| TextResultError::Invalid)?;
            objects
                .put(&object, &content)
                .await
                .map_err(|_| TextResultError::Unavailable)?;
            cancelled(&cancellation)?;
            backend!(pool, |pool, schema| {
                let mut tx = pool.begin().await.map_err(sql_error)?;
                // The first transaction statement obtains the same invocation-row lock
                // as terminalization. No source IO occurs while this lock is held.
                let changed = sqlx::query(&format!("UPDATE {schema}script_invocations SET status=status WHERE id=$1 AND status='running'"))
                    .bind(scope.invocation_id()).execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
                if changed != 1 {
                    return Err(TextResultError::Conflict);
                }
                #[cfg(test)]
                evidence_hooks::visit(scope.invocation_id(), evidence_hooks::Stage::Locked).await;
                let owner: (String,) = sqlx::query_as(&format!(
                    "SELECT record FROM {schema}script_invocations WHERE id=$1"
                ))
                .bind(scope.invocation_id())
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
                check_owner(&owner.0, &scope)?;
                let existing: Option<(String,)> = sqlx::query_as(&format!(
                    "SELECT id FROM {schema}script_text_results WHERE invocation=$1 AND name=$2"
                ))
                .bind(scope.invocation_id())
                .bind(&descriptor.name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sql_error)?;
                if existing.is_some() {
                    return Err(TextResultError::Conflict);
                }
                sqlx::query(&format!("INSERT INTO {schema}script_text_result_usage (invocation,encoded_bytes,result_count) VALUES ($1,0,0) ON CONFLICT (invocation) DO NOTHING"))
                    .bind(scope.invocation_id()).execute(&mut *tx).await.map_err(sql_error)?;
                let changed = sqlx::query(&format!("UPDATE {schema}script_text_result_usage SET encoded_bytes=encoded_bytes+$2,result_count=result_count+1 WHERE invocation=$1 AND encoded_bytes<=$3 AND result_count<$4"))
                    .bind(scope.invocation_id()).bind(charge).bind(remaining).bind(i64::try_from(MAX_RESULTS).map_err(|_| TextResultError::Quota)?)
                    .execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
                if changed != 1 {
                    return Err(TextResultError::Quota);
                }
                #[cfg(test)]
                evidence_hooks::visit(scope.invocation_id(), evidence_hooks::Stage::Charged).await;
                sqlx::query(&format!("INSERT INTO {schema}script_text_results (id,invocation,name,descriptor,utf8_bytes,encoded_charge,object_sha256) VALUES ($1,$2,$3,$4,$5,$6,$7)"))
                    .bind(&descriptor.id).bind(scope.invocation_id()).bind(&descriptor.name).bind(&encoded)
                    .bind(i64::try_from(content.len()).map_err(|_| TextResultError::Quota)?).bind(charge).bind(&object.sha256)
                    .execute(&mut *tx).await.map_err(sql_error)?;
                #[cfg(test)]
                evidence_hooks::visit(
                    scope.invocation_id(),
                    evidence_hooks::Stage::BeforeCancelCheck,
                )
                .await;
                cancelled(&cancellation)?;
                #[cfg(test)]
                evidence_hooks::visit(
                    scope.invocation_id(),
                    evidence_hooks::Stage::PassedCancelCheck,
                )
                .await;
                // Never race cancellation against commit: an error here is ambiguous.
                tx.commit().await.map_err(|_| TextResultError::Uncertain)?;
                #[cfg(test)]
                if evidence_hooks::visit(scope.invocation_id(), evidence_hooks::Stage::Committed)
                    .await
                {
                    return Err(TextResultError::Uncertain);
                }
                Ok(descriptor)
            })
        })
    }
    fn list(&self, scope: InvocationTextScope) -> TextFuture<Vec<TextResultDescriptor>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            authorize(&pool, &scope).await?;
            backend!(pool, |pool, schema| {
                let rows: Vec<(String,)> = sqlx::query_as(&format!("SELECT descriptor FROM {schema}script_text_results WHERE invocation=$1 ORDER BY name LIMIT 32"))
                    .bind(scope.invocation_id()).fetch_all(&pool).await.map_err(sql_error)?;
                rows.into_iter()
                    .map(|row| decode_descriptor(&row.0))
                    .collect()
            })
        })
    }
    fn read(
        &self,
        scope: InvocationTextScope,
        id: String,
        offset: u64,
        max: u32,
        cancellation: CancellationToken,
    ) -> TextFuture<TextChunk> {
        let pool = self.pool.clone();
        let objects = self.objects.clone();
        Box::pin(async move {
            authorize(&pool, &scope).await?;
            if max == 0
                || u64::from(max) > MAX_CHUNK_BYTES as u64
                || id.len() != 36
                || id.capacity() > 256
                || uuid::Uuid::parse_str(&id).is_err()
            {
                return Err(TextResultError::Invalid);
            }
            backend!(pool, |pool, schema| {
                let row: Option<(i64, String, String)> = sqlx::query_as(&format!("SELECT utf8_bytes,object_sha256,descriptor FROM {schema}script_text_results WHERE invocation=$1 AND id=$2"))
                    .bind(scope.invocation_id()).bind(&id).fetch_optional(&pool).await.map_err(sql_error)?;
                let (size, digest, descriptor) = row.ok_or(TextResultError::Denied)?;
                let size = u64::try_from(size).map_err(|_| TextResultError::Unavailable)?;
                if offset > size || offset.checked_add(u64::from(max)).is_none() {
                    return Err(TextResultError::Invalid);
                }
                let descriptor = decode_descriptor(&descriptor)?;
                if descriptor.sha256 != digest
                    || descriptor.utf8_bytes != size
                    || descriptor.id != id
                {
                    return Err(TextResultError::Unavailable);
                }
                let object = crate::routine_objects::ObjectRef::new(
                    crate::routine_objects::Namespace::Text,
                    digest,
                    size,
                )
                .map_err(|_| TextResultError::Unavailable)?;
                let _working =
                    reserve((MAX_TEXT_BYTES + MAX_CHUNK_BYTES) as u32, &cancellation).await?;
                let content = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(TextResultError::Cancelled),
                    bytes = objects.get(&object) => bytes.map_err(|error| {
                        tracing::warn!(%error, "published Routine text object could not be verified");
                        TextResultError::Unavailable
                    })?,
                };
                std::str::from_utf8(&content).map_err(|_| TextResultError::Unavailable)?;
                let credit = reserve(MAX_CHUNK_BYTES as u32, &cancellation).await?;
                let end = (offset as usize + max as usize).min(content.len());
                let bytes = content[offset as usize..end].to_vec();
                Ok(TextChunk::new(bytes, credit))
            })
        })
    }
}

#[cfg(test)]
mod evidence_hooks;
#[cfg(test)]
mod tests;
