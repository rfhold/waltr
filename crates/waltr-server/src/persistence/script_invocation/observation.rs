use super::*;
use crate::routine::observation::{
    self, InvocationView, Observation, ObservationPage, ObservationScope, ObservationStore,
    OperationView, Publication,
};

type Row = (
    i64,
    String,
    i64,
    String,
    String,
    i64,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
);
const COLUMNS: &str = "j.cursor,j.kind,j.admission_cursor,j.record,j.status,j.updated_ms,j.result,j.text_results,j.source_session,r.id,r.api_version,r.engine,('{\"sha256\":\"' || r.object_sha256 || '\",\"bytes\":' || CAST(r.object_bytes AS TEXT) || '}'),r.name,i.public_definition,j.publications";

async fn hydrate(mut row: Row, objects: &dyn crate::routine_objects::ImmutableObjects) -> Result<Row, PortError> {
    if row.1 == "invocation" {
        let encoded: ProgramReference = decode(&row.12)?;
        let object = crate::routine_objects::ObjectRef::new(crate::routine_objects::Namespace::Program, encoded.sha256, encoded.bytes)
            .map_err(|_| PortError::Unavailable)?;
        let bytes = objects.get(&object).await.map_err(|error| {
            tracing::warn!(%error, "historical Routine program object could not be verified");
            PortError::Unavailable
        })?;
        row.12 = String::from_utf8(bytes).map_err(|_| PortError::Unavailable)?;
        if revision_id(&row.11, &row.12) != row.9 { return Err(PortError::Unavailable); }
    }
    Ok(row)
}

#[derive(serde::Deserialize)]
struct ProgramReference { sha256: String, bytes: u64 }

fn observed(row: Row) -> Result<Observation, PortError> {
    let mutation = row.0;
    let record = (row.2, row.3, row.4, row.5, row.6);
    match row.1.as_str() {
        "invocation" => Ok(Observation::Invocation(InvocationView {
            invocation: invocation(record)?,
            observation_revision: mutation,
            source_session: row.8,
            text_results: decode(&row.7)?,
            revision: crate::routine::history::Revision {
                id: row.9,
                api_version: row.10,
                engine: row.11,
                envelope: row.12,
                name: row.13,
            },
            public_definition: row.14.as_deref().map(decode).transpose()?,
            publications: decode(&row.15)?,
        })),
        "operation" => Ok(Observation::Operation(OperationView {
            operation: operation(record)?,
            observation_revision: mutation,
        })),
        _ => Err(PortError::Unavailable),
    }
}
fn only_invocation(row: Row) -> Result<InvocationView, PortError> {
    match observed(row)? {
        Observation::Invocation(row) => Ok(row),
        _ => Err(PortError::Unavailable),
    }
}
fn only_operation(row: Row) -> Result<OperationView, PortError> {
    match observed(row)? {
        Observation::Operation(row) => Ok(row),
        _ => Err(PortError::Unavailable),
    }
}
fn tables(schema: &str) -> String {
    format!(
        "{schema}script_observations j JOIN {schema}script_invocations i ON i.id=j.invocation JOIN {schema}script_program_revisions r ON r.id=i.revision"
    )
}

impl ObservationStore for SqlInvocationStore {
    fn publish(
        &self,
        scope: ObservationScope,
        id: String,
        publication: Publication,
    ) -> PortFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            observation::validate(&scope, 0, 1)?;
            let valid = |value: &str, max: usize| !value.is_empty() && value.len() <= max;
            match &publication {
                Publication::Helper {
                    session_id,
                    definition_identity,
                } if valid(session_id, 256) && valid(definition_identity, 256) => {}
                Publication::Blob {
                    blob_id,
                    port,
                    display_name,
                    content_type,
                    bytes,
                    image,
                    remaining_validity_ms,
                    ..
                } if valid(blob_id, 256)
                    && valid(port, 128)
                    && valid(display_name, 255)
                    && valid(content_type, 255)
                    && *bytes <= 64 * 1024 * 1024
                    && *remaining_validity_ms <= 900000
                    && image.is_none_or(|(format, width, height)| {
                        (1..=3).contains(&format)
                            && width > 0
                            && width <= 8192
                            && height > 0
                            && height <= 8192
                            && u64::from(width) * u64::from(height) <= 32000000
                    }) => {}
                _ => return Err(PortError::Invalid),
            }
            backend!(pool, |pool, schema| {
                let mut tx = pool.begin().await.map_err(sql_error)?;
                let owner = if schema.is_empty() {
                    "json_extract(record,'$.caller.principal')"
                } else {
                    "record::jsonb->'caller'->>'principal'"
                };
                let changed=sqlx::query(&format!("UPDATE {schema}script_invocations SET status=status WHERE id=$1 AND {owner}=$2 AND (CAST($3 AS TEXT) IS NULL OR source_session=$3) AND status='running'"))
                    .bind(&id).bind(scope.principal).bind(scope.source_session).execute(&mut *tx).await.map_err(sql_error)?;
                if changed.rows_affected() != 1 {
                    return Err(PortError::Denied);
                }
                let previous:Option<(String,)>=sqlx::query_as(&format!("SELECT publications FROM {schema}script_invocation_publications WHERE invocation=$1")).bind(&id).fetch_optional(&mut *tx).await.map_err(sql_error)?;
                let mut publications: Vec<Publication> = previous
                    .map(|(value,)| decode(&value))
                    .transpose()?
                    .unwrap_or_default();
                if publications.contains(&publication) {
                    return Err(PortError::Conflict);
                }
                if publications.len() >= 64 {
                    return Err(PortError::Unavailable);
                }
                publications.push(publication);
                let value = encode(&publications, 65536)?;
                sqlx::query(&format!("INSERT INTO {schema}script_invocation_publications (invocation,publications) VALUES ($1,$2) ON CONFLICT (invocation) DO UPDATE SET publications=excluded.publications"))
                    .bind(id).bind(value).execute(&mut *tx).await.map_err(sql_error)?;
                tx.commit().await.map_err(sql_error)?;
                Ok(())
            })
        })
    }
    fn cursor_key(&self) -> PortFuture<[u8; 32]> {
        let pool = self.pool.clone();
        Box::pin(async move {
            backend!(pool, |pool, schema| {
                let (key,): (Vec<u8>,) = sqlx::query_as(&format!(
                    "SELECT cursor_key FROM {schema}script_observation_head WHERE singleton=1"
                ))
                .fetch_one(&pool)
                .await
                .map_err(sql_error)?;
                key.try_into().map_err(|_| PortError::Unavailable)
            })
        })
    }
    fn get(&self, scope: ObservationScope, id: String) -> PortFuture<Option<InvocationView>> {
        let pool = self.pool.clone();
        let objects = self.objects.clone();
        Box::pin(async move {
            observation::validate(&scope, 0, 1)?;
            if id.is_empty() || id.len() > 256 {
                return Err(PortError::Invalid);
            }
            backend!(pool, |pool, schema| {
                let row = sqlx::query_as::<_,Row>(&format!("SELECT {COLUMNS} FROM {} WHERE j.principal=$1 AND (CAST($2 AS TEXT) IS NULL OR j.source_session=$2) AND j.kind='invocation' AND j.entity=$3 ORDER BY j.cursor DESC LIMIT 1",tables(schema)))
                    .bind(scope.principal).bind(scope.source_session).bind(id).fetch_optional(&pool).await.map_err(sql_error)?;
                match row { Some(row) => Ok(Some(only_invocation(hydrate(row, objects.as_ref()).await?)?)), None => Ok(None) }
            })
        })
    }
    fn list(
        &self,
        scope: ObservationScope,
        after: i64,
        limit: u32,
    ) -> PortFuture<Vec<InvocationView>> {
        let pool = self.pool.clone();
        let objects = self.objects.clone();
        Box::pin(async move {
            observation::validate(&scope, after, limit)?;
            backend!(pool, |pool, schema| {
                let rows = sqlx::query_as::<_,Row>(&format!("SELECT {COLUMNS} FROM {} WHERE j.principal=$1 AND (CAST($2 AS TEXT) IS NULL OR j.source_session=$2) AND j.kind='invocation' AND j.admission_cursor>$3 AND NOT EXISTS (SELECT 1 FROM {schema}script_observations newer WHERE newer.principal=j.principal AND newer.kind=j.kind AND newer.entity=j.entity AND newer.cursor>j.cursor) ORDER BY j.admission_cursor LIMIT $4",tables(schema)))
                    .bind(scope.principal).bind(scope.source_session).bind(after).bind(i64::from(limit)).fetch_all(&pool).await.map_err(sql_error)?;
                let mut result = Vec::with_capacity(rows.len());
                for row in rows { result.push(only_invocation(hydrate(row, objects.as_ref()).await?)?); }
                Ok(result)
            })
        })
    }
    fn operations(
        &self,
        scope: ObservationScope,
        parent: String,
        after: i64,
        limit: u32,
    ) -> PortFuture<Vec<OperationView>> {
        let pool = self.pool.clone();
        let store = self.clone();
        Box::pin(async move {
            observation::validate(&scope, after, limit)?;
            if ObservationStore::get(&store, scope.clone(), parent.clone())
                .await?
                .is_none()
            {
                return Err(PortError::Denied);
            }
            backend!(pool, |pool, schema| {
                sqlx::query_as::<_,Row>(&format!("SELECT {COLUMNS} FROM {} WHERE j.principal=$1 AND (CAST($2 AS TEXT) IS NULL OR j.source_session=$2) AND j.kind='operation' AND j.invocation=$3 AND j.admission_cursor>$4 AND NOT EXISTS (SELECT 1 FROM {schema}script_observations newer WHERE newer.principal=j.principal AND newer.kind=j.kind AND newer.entity=j.entity AND newer.cursor>j.cursor) ORDER BY j.admission_cursor LIMIT $5",tables(schema)))
                    .bind(scope.principal).bind(scope.source_session).bind(parent).bind(after).bind(i64::from(limit)).fetch_all(&pool).await.map_err(sql_error)?.into_iter().map(only_operation).collect()
            })
        })
    }
    fn page(
        &self,
        scope: ObservationScope,
        high_water: Option<i64>,
        after: i64,
        snapshot: bool,
        limit: u32,
    ) -> PortFuture<ObservationPage> {
        let pool = self.pool.clone();
        let objects = self.objects.clone();
        Box::pin(async move {
            observation::validate(&scope, after, limit)?;
            backend!(pool, |pool, schema| {
                let (head,): (i64,) = sqlx::query_as(&format!(
                    "SELECT cursor FROM {schema}script_observation_head WHERE singleton=1"
                ))
                .fetch_one(&pool)
                .await
                .map_err(sql_error)?;
                let high_water = high_water.unwrap_or(head);
                if high_water < 0
                    || high_water > head
                    || (snapshot && after > high_water)
                    || (!snapshot && after > head)
                {
                    return Err(PortError::Conflict);
                }
                let upper = if snapshot { high_water } else { head };
                let latest = if snapshot {
                    format!(
                        "AND NOT EXISTS (SELECT 1 FROM {schema}script_observations newer WHERE newer.principal=j.principal AND newer.kind=j.kind AND newer.entity=j.entity AND newer.cursor>j.cursor AND newer.cursor<=$4)"
                    )
                } else {
                    String::new()
                };
                let rows=sqlx::query_as::<_,Row>(&format!("SELECT {COLUMNS} FROM {} WHERE j.principal=$1 AND (CAST($2 AS TEXT) IS NULL OR j.source_session=$2) AND j.cursor>$3 AND j.cursor<=$4 {latest} ORDER BY j.cursor LIMIT $5",tables(schema)))
                    .bind(scope.principal).bind(scope.source_session).bind(after).bind(upper).bind(i64::from(limit)+1).fetch_all(&pool).await.map_err(sql_error)?;
                let complete = rows.len() <= limit as usize;
                let rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
                let position = if complete {
                    upper
                } else {
                    rows.last().map_or(after, |row| row.0)
                };
                let mut result = Vec::with_capacity(rows.len());
                for row in rows { result.push(observed(hydrate(row, objects.as_ref()).await?)?); }
                Ok(ObservationPage {
                    rows: result,
                    position,
                    high_water,
                    complete,
                })
            })
        })
    }
}
