//! Additive Waltr SQL history. No database types cross InvocationStore.
use super::PersistencePool;
use crate::routine::{
    definition::{API_VERSION, MAX_ENVELOPE_BYTES, identifier, revision_id},
    history::{Admission, Invocation, Operation, OperationStatus, Status},
    ports::{InvocationStore, PortError, PortFuture},
    service::bounded_projection,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Clone)]
/// Shared-pool SQLite/PostgreSQL implementation of the consumer-owned history port.
pub struct SqlInvocationStore {
    pool: PersistencePool,
    objects: std::sync::Arc<dyn crate::routine_objects::ImmutableObjects>,
}
impl SqlInvocationStore {
    pub(super) fn with_objects(pool: PersistencePool, objects: std::sync::Arc<dyn crate::routine_objects::ImmutableObjects>) -> Self {
        Self { pool, objects }
    }
    #[cfg(test)]
    pub(super) fn new(pool: PersistencePool) -> Self {
        Self::with_objects(pool, crate::routine_objects::test_objects())
    }
}
impl std::fmt::Debug for SqlInvocationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlInvocationStore")
            .field("backend", &self.pool.backend())
            .finish_non_exhaustive()
    }
}

// Both backends run the same statements and transactions. $N parameters are
// accepted by SQLite as well; PostgreSQL names are always schema-qualified.
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
mod observation;

fn sql_error(_: sqlx::Error) -> PortError {
    PortError::Unavailable
}
fn encode<T: Serialize>(value: &T, cap: usize) -> Result<String, PortError> {
    let result = serde_json::to_string(value).map_err(|_| PortError::Invalid)?;
    if result.len() > cap {
        return Err(PortError::Invalid);
    }
    Ok(result)
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T, PortError> {
    serde_json::from_str(value).map_err(|_| PortError::Unavailable)
}
fn result_json(value: &Option<Value>, cap: usize) -> Result<Option<String>, PortError> {
    value
        .as_ref()
        .map(|v| {
            if cap == 4096 {
                bounded_projection(v)?;
            } else {
                waltr_script::StructuredValue::new(v.clone(), &waltr_script::Limits::default())
                    .map_err(|_| PortError::Invalid)?;
            }
            encode(v, cap)
        })
        .transpose()
}
fn changed(rows: u64) -> Result<(), PortError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(PortError::Conflict)
    }
}
fn status(value: &str) -> Result<Status, PortError> {
    decode(&format!("\"{value}\""))
}
type InvocationRow = (i64, String, String, i64, Option<String>);
fn invocation(row: InvocationRow) -> Result<Invocation, PortError> {
    let mut record: Invocation = decode(&row.1)?;
    if record.version != 1 {
        return Err(PortError::Unavailable);
    }
    record.cursor = row.0;
    record.status = status(&row.2)?;
    record.updated_ms = row.3;
    record.result = row.4.as_deref().map(decode).transpose()?;
    Ok(record)
}
type OperationRow = (i64, String, String, i64, Option<String>);
fn operation(row: OperationRow) -> Result<Operation, PortError> {
    let mut record: Operation = decode(&row.1)?;
    if record.version != 1 {
        return Err(PortError::Unavailable);
    }
    record.cursor = row.0;
    record.status = decode(&format!("\"{}\"", row.2))?;
    record.updated_ms = row.3;
    record.result = row.4.as_deref().map(decode).transpose()?;
    Ok(record)
}

impl InvocationStore for SqlInvocationStore {
    fn admit(&self, admission: Admission) -> PortFuture<()> {
        self.admit_with_source(admission, None)
    }
    fn admit_with_source(
        &self,
        admission: Admission,
        source_session: Option<String>,
    ) -> PortFuture<()> {
        self.admit_with_projection(admission, source_session, None)
    }
    fn admit_with_projection(
        &self,
        admission: Admission,
        source_session: Option<String>,
        public_definition: Option<Value>,
    ) -> PortFuture<()> {
        let pool = self.pool.clone();
        let objects = self.objects.clone();
        Box::pin(async move {
            let revision = &admission.revision;
            if !identifier(&admission.id)
                || revision.api_version != API_VERSION
                || !identifier(&revision.name)
                || revision.engine.len() > 256
                || revision.envelope.len() > MAX_ENVELOPE_BYTES
                || revision.id != revision_id(&revision.engine, &revision.envelope)
            {
                return Err(PortError::Invalid);
            }
            let record = Invocation {
                version: 1,
                cursor: 0,
                id: admission.id.clone(),
                revision: revision.id.clone(),
                caller: admission.caller,
                entrypoint: admission.entrypoint,
                snapshot: admission.snapshot,
                status: Status::Admitted,
                admitted_ms: admission.admitted_ms,
                updated_ms: admission.admitted_ms,
                result: None,
            };
            let record = encode(&record, 4096)?;
            let object = crate::routine_objects::ObjectRef::for_bytes(crate::routine_objects::Namespace::Program, revision.envelope.as_bytes())
                .map_err(|_| PortError::Invalid)?;
            objects.put(&object, revision.envelope.as_bytes()).await.map_err(|_| PortError::Unavailable)?;
            backend!(pool, |pool, schema| {
                let mut tx = pool.begin().await.map_err(sql_error)?;
                sqlx::query(&format!("INSERT INTO {schema}script_program_revisions (id,api_version,engine,object_sha256,name,object_bytes) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (id) DO NOTHING"))
                    .bind(&revision.id).bind(&revision.api_version).bind(&revision.engine).bind(&object.sha256).bind(&revision.name).bind(object.bytes as i64).execute(&mut *tx).await.map_err(sql_error)?;
                let existing: (String, String, String, String, i64) = sqlx::query_as(&format!("SELECT api_version,engine,object_sha256,name,object_bytes FROM {schema}script_program_revisions WHERE id=$1"))
                    .bind(&revision.id).fetch_one(&mut *tx).await.map_err(sql_error)?;
                if existing
                    != (
                        revision.api_version.clone(),
                        revision.engine.clone(),
                        object.sha256.clone(),
                        revision.name.clone(),
                        object.bytes as i64,
                    )
                {
                    return Err(PortError::Conflict);
                }
                if source_session.is_some() || public_definition.is_some() {
                    if source_session
                        .as_ref()
                        .is_some_and(|source| source.is_empty() || source.len() > 256)
                    {
                        return Err(PortError::Invalid);
                    }
                    let projection = public_definition
                        .as_ref()
                        .map(|value| encode(value, 65536))
                        .transpose()?;
                    sqlx::query(&format!("INSERT INTO {schema}script_invocations (id,revision,record,status,admitted_ms,updated_ms,source_session,public_definition) VALUES ($1,$2,$3,'admitted',$4,$4,$5,$6)"))
                        .bind(&admission.id).bind(&revision.id).bind(&record).bind(admission.admitted_ms).bind(source_session).bind(projection).execute(&mut *tx).await.map_err(sql_error)?;
                } else {
                    sqlx::query(&format!("INSERT INTO {schema}script_invocations (id,revision,record,status,admitted_ms,updated_ms) VALUES ($1,$2,$3,'admitted',$4,$4)"))
                        .bind(&admission.id).bind(&revision.id).bind(&record).bind(admission.admitted_ms).execute(&mut *tx).await.map_err(sql_error)?;
                }
                tx.commit().await.map_err(|_| PortError::Uncertain)
            })
        })
    }
    fn running(&self, id: String, now: i64) -> PortFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            backend!(pool, |pool, schema| {
                changed(sqlx::query(&format!("UPDATE {schema}script_invocations SET status='running',updated_ms=$2 WHERE id=$1 AND status='admitted'"))
                .bind(&id).bind(now).execute(&pool).await.map_err(sql_error)?.rows_affected())
            })
        })
    }
    fn dispatch_intent(&self, operation: Operation) -> PortFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            if operation.version != 1
                || operation.status != OperationStatus::Dispatched
                || operation.result.is_some()
                || !identifier(&operation.id)
                || !identifier(&operation.name)
            {
                return Err(PortError::Invalid);
            }
            bounded_projection(&operation.arguments)?;
            let record = encode(&operation, 8192)?;
            backend!(pool, |pool, schema| {
                let mut tx = pool.begin().await.map_err(sql_error)?;
                // Serializes dispatch against terminalization in both backends.
                changed(sqlx::query(&format!("UPDATE {schema}script_invocations SET status=status WHERE id=$1 AND status='running'"))
                    .bind(&operation.invocation).execute(&mut *tx).await.map_err(sql_error)?.rows_affected())?;
                let count: (i64,) = sqlx::query_as(&format!(
                    "SELECT COUNT(*) FROM {schema}script_operations WHERE invocation=$1"
                ))
                .bind(&operation.invocation)
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
                if count.0 >= 1024 {
                    return Err(PortError::Invalid);
                }
                sqlx::query(&format!("INSERT INTO {schema}script_operations (id,invocation,record,status,updated_ms) VALUES ($1,$2,$3,'dispatched',$4)"))
                    .bind(&operation.id).bind(&operation.invocation).bind(&record).bind(operation.updated_ms).execute(&mut *tx).await.map_err(sql_error)?;
                tx.commit().await.map_err(sql_error)
            })
        })
    }
    fn operation_done(
        &self,
        id: String,
        state: OperationStatus,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<()> {
        let pool = self.pool.clone();
        Box::pin(async move {
            if state == OperationStatus::Dispatched {
                return Err(PortError::Invalid);
            }
            let result = result_json(&result, 4096)?;
            backend!(pool, |pool, schema| {
                let mut tx = pool.begin().await.map_err(sql_error)?;
                let locked=sqlx::query(&format!("UPDATE {schema}script_invocations SET status=status WHERE id=(SELECT invocation FROM {schema}script_operations WHERE id=$1) AND status='running'"))
                    .bind(&id).execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
                let _parent: (String,) = sqlx::query_as(&format!(
                    "SELECT invocation FROM {schema}script_operations WHERE id=$1"
                ))
                .bind(&id)
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
                changed(locked)?;
                changed(sqlx::query(&format!("UPDATE {schema}script_operations SET status=$2,result=$3,updated_ms=$4 WHERE id=$1 AND status='dispatched'"))
                    .bind(&id).bind(state.as_str()).bind(&result).bind(now).execute(&mut *tx).await.map_err(sql_error)?.rows_affected())?;
                tx.commit().await.map_err(sql_error)
            })
        })
    }
    fn terminal(
        &self,
        id: String,
        state: Status,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<Status> {
        let pool = self.pool.clone();
        Box::pin(async move {
            if !state.terminal() || (state != Status::Succeeded && result.is_some()) {
                return Err(PortError::Invalid);
            }
            let result = result_json(&result, 65_536)?;
            backend!(pool, |pool, schema| {
                let mut tx = pool.begin().await.map_err(sql_error)?;
                changed(sqlx::query(&format!("UPDATE {schema}script_invocations SET status=status WHERE id=$1 AND status IN ('admitted','running')"))
                    .bind(&id).execute(&mut *tx).await.map_err(sql_error)?.rows_affected())?;
                sqlx::query(&format!("UPDATE {schema}script_operations SET status='uncertain',updated_ms=$2 WHERE invocation=$1 AND status='dispatched'"))
                    .bind(&id).bind(now).execute(&mut *tx).await.map_err(sql_error)?;
                let uncertain: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {schema}script_operations WHERE invocation=$1 AND status='uncertain'"))
                    .bind(&id).fetch_one(&mut *tx).await.map_err(sql_error)?;
                let actual = if uncertain.0 > 0 {
                    Status::Uncertain
                } else {
                    state
                };
                let result = if actual == Status::Succeeded {
                    result.clone()
                } else {
                    None
                };
                changed(sqlx::query(&format!("UPDATE {schema}script_invocations SET status=$2,result=$3,updated_ms=$4 WHERE id=$1 AND status IN ('admitted','running')"))
                    .bind(&id).bind(actual.as_str()).bind(&result).bind(now).execute(&mut *tx).await.map_err(sql_error)?.rows_affected())?;
                tx.commit().await.map_err(sql_error)?;
                Ok(actual)
            })
        })
    }
    fn get(&self, id: String) -> PortFuture<Option<Invocation>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            backend!(pool, |pool, schema| {
                sqlx::query_as::<_, InvocationRow>(&format!("SELECT cursor,record,status,updated_ms,result FROM {schema}script_invocations WHERE id=$1"))
                .bind(&id).fetch_optional(&pool).await.map_err(sql_error)?.map(invocation).transpose()
            })
        })
    }
    fn list(&self, after: i64, limit: u32) -> PortFuture<Vec<Invocation>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            if after < 0 || limit == 0 || limit > 100 {
                return Err(PortError::Invalid);
            }
            backend!(pool, |pool, schema| {
                sqlx::query_as::<_, InvocationRow>(&format!("SELECT cursor,record,status,updated_ms,result FROM {schema}script_invocations WHERE cursor>$1 ORDER BY cursor LIMIT $2"))
                    .bind(after).bind(i64::from(limit)).fetch_all(&pool).await.map_err(sql_error)?.into_iter().map(invocation).collect()
            })
        })
    }
    fn operations(&self, parent: String, after: i64, limit: u32) -> PortFuture<Vec<Operation>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            if limit == 0 || limit > 100 || after < 0 {
                return Err(PortError::Invalid);
            }
            backend!(pool, |pool, schema| {
                sqlx::query_as::<_, OperationRow>(&format!("SELECT cursor,record,status,updated_ms,result FROM {schema}script_operations WHERE invocation=$1 AND cursor>$2 ORDER BY cursor LIMIT $3"))
                    .bind(&parent).bind(after).bind(i64::from(limit)).fetch_all(&pool).await.map_err(sql_error)?.into_iter().map(operation).collect()
            })
        })
    }
    fn reconcile(&self, now: i64) -> PortFuture<u64> {
        let pool = self.pool.clone();
        Box::pin(async move {
            backend!(pool, |pool, schema| {
                let mut tx = pool.begin().await.map_err(sql_error)?;
                sqlx::query(&format!("UPDATE {schema}script_operations SET status='uncertain',updated_ms=$1 WHERE status='dispatched'"))
                .bind(now).execute(&mut *tx).await.map_err(sql_error)?;
                let count = sqlx::query(&format!("UPDATE {schema}script_invocations SET status=CASE WHEN EXISTS (SELECT 1 FROM {schema}script_operations o WHERE o.invocation={schema}script_invocations.id AND o.status='uncertain') THEN 'uncertain' ELSE 'interrupted' END,updated_ms=$1 WHERE status IN ('admitted','running')"))
                .bind(now).execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
                tx.commit().await.map_err(sql_error)?;
                Ok(count)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::history::{Caller, Origin, Ownership, Revision};
    use sqlx::{Executor, postgres::PgPoolOptions, sqlite::SqlitePoolOptions};

    fn admission(id: &str) -> Admission {
        let engine = "test-js;test-engine;abi=1";
        let envelope = r#"{"apiVersion":"waltr.routine/v1","kind":"Routine","name":"test","spec":{"source":"export default async function(input) { return input; }"}}"#;
        Admission {
            id: id.into(),
            revision: Revision {
                id: revision_id(engine, envelope),
                api_version: API_VERSION.into(),
                engine: engine.into(),
                envelope: envelope.into(),
                name: "test".into(),
            },
            caller: Caller {
                principal: "test".into(),
                origin: Origin::Manual,
                ownership: Ownership::Detached,
                correlation: "correlation".into(),
            },
            entrypoint: "run".into(),
            snapshot: Some("snapshot-v1".into()),
            admitted_ms: 100,
        }
    }
    fn intent(parent: &str, id: &str) -> Operation {
        Operation {
            version: 1,
            cursor: 0,
            id: id.into(),
            invocation: parent.into(),
            name: "echo".into(),
            status: OperationStatus::Dispatched,
            arguments: serde_json::json!({"redacted":true}),
            result: None,
            updated_ms: 102,
        }
    }
    async fn contract(store: &SqlInvocationStore) {
        store.admit(admission("first")).await.unwrap();
        store.admit(admission("second")).await.unwrap();
        assert!(store.admit(admission("first")).await.is_err());
        let rows = store.list(0, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "first");
        let next = store.list(rows[0].cursor, 1).await.unwrap();
        assert_eq!(next[0].id, "second");
        assert!(store.list(0, 101).await.is_err());
        assert!(store.list(-1, 1).await.is_err());
        assert!(
            store
                .dispatch_intent(intent("first", "early"))
                .await
                .is_err()
        );
        let (a, b) = tokio::join!(
            store.running("first".into(), 101),
            store.running("first".into(), 101)
        );
        assert_ne!(
            a.is_ok(),
            b.is_ok(),
            "running must be a durable compare-and-swap"
        );
        store
            .dispatch_intent(intent("first", "operation-z"))
            .await
            .unwrap();
        store
            .operation_done(
                "operation-z".into(),
                OperationStatus::Completed,
                Some(serde_json::json!("safe")),
                103,
            )
            .await
            .unwrap();
        assert!(
            store
                .operation_done("operation-z".into(), OperationStatus::Failed, None, 104)
                .await
                .is_err()
        );
        // Pagination follows durable insertion cursors, not random operation IDs.
        store
            .dispatch_intent(intent("first", "operation-a"))
            .await
            .unwrap();
        let first_op = store.operations("first".into(), 0, 1).await.unwrap();
        assert_eq!(first_op[0].id, "operation-z");
        let next_op = store
            .operations("first".into(), first_op[0].cursor, 1)
            .await
            .unwrap();
        assert_eq!(next_op[0].id, "operation-a");
        assert_eq!(
            store
                .terminal(
                    "first".into(),
                    Status::Succeeded,
                    Some(serde_json::json!("candidate")),
                    105
                )
                .await
                .unwrap(),
            Status::Uncertain
        );
        let row = store.get("first".into()).await.unwrap().unwrap();
        assert_eq!(row.status, Status::Uncertain);
        assert!(row.result.is_none());
        assert!(
            store
                .terminal("first".into(), Status::Succeeded, None, 106)
                .await
                .is_err()
        );
        assert!(
            store
                .dispatch_intent(intent("first", "late"))
                .await
                .is_err()
        );
        assert_eq!(store.reconcile(110).await.unwrap(), 1);
        assert_eq!(store.reconcile(111).await.unwrap(), 0);
        assert_eq!(
            store.get("second".into()).await.unwrap().unwrap().status,
            Status::Interrupted
        );
        assert_eq!(
            store.operations("first".into(), 0, 100).await.unwrap()[1].status,
            OperationStatus::Uncertain
        );

        store.admit(admission("race")).await.unwrap();
        store.running("race".into(), 112).await.unwrap();
        let (dispatched, terminal) = tokio::join!(
            store.dispatch_intent(intent("race", "racing-operation")),
            store.terminal("race".into(), Status::Succeeded, None, 114)
        );
        let terminal = terminal.unwrap();
        if dispatched.is_ok() {
            assert_eq!(terminal, Status::Uncertain);
        } else {
            assert_eq!(terminal, Status::Succeeded);
        }

        store.admit(admission("unfinished")).await.unwrap();
        store.running("unfinished".into(), 115).await.unwrap();
        store
            .dispatch_intent(intent("unfinished", "uncertain-effect"))
            .await
            .unwrap();
        assert_eq!(store.reconcile(116).await.unwrap(), 1);
        assert_eq!(
            store
                .get("unfinished".into())
                .await
                .unwrap()
                .unwrap()
                .status,
            Status::Uncertain
        );
        assert_eq!(store.reconcile(117).await.unwrap(), 0);
        assert_eq!(
            store.operations("unfinished".into(), 0, 100).await.unwrap()[0].status,
            OperationStatus::Uncertain
        );

        store.admit(admission("large-result")).await.unwrap();
        assert!(
            store
                .terminal(
                    "large-result".into(),
                    Status::Succeeded,
                    Some(Value::String("x".repeat(65_537))),
                    118
                )
                .await
                .is_err()
        );
        assert_eq!(
            store
                .get("large-result".into())
                .await
                .unwrap()
                .unwrap()
                .status,
            Status::Admitted
        );
    }
    const OLD_HISTORY: &str = "INSERT INTO routine_invocations (invocation_id,execution_id,routine_name,routine_revision,trigger_name,trigger_kind,correlation_id,declared_tasks_json,status,admitted_at_ms,updated_at_ms) VALUES ('legacy-sentinel','execution','legacy','revision','manual','manual','correlation','[]','admitted',1,1)";

    #[tokio::test]
    async fn historical_program_reads_exact_object_and_never_catalog_fallback() {
        use crate::routine::observation::{ObservationScope, ObservationStore};
        let root = crate::test_support::temp_root().join("objects");
        let objects = std::sync::Arc::new(crate::routine_objects::FilesystemObjects::new(root.clone()).unwrap());
        let persistence = super::super::Persistence::initialize_with_objects(super::super::DatabaseConfig::for_tests().unwrap(), objects).await.unwrap();
        let store = persistence.script_invocation_store();
        let admission = admission("historical-object");
        let principal = admission.caller.principal.clone();
        let envelope = admission.revision.envelope.clone();
        let reference = crate::routine_objects::ObjectRef::for_bytes(crate::routine_objects::Namespace::Program, envelope.as_bytes()).unwrap();
        store.admit(admission).await.unwrap();
        let scope = ObservationScope { principal, source_session: None };
        let read = ObservationStore::get(store.as_ref(), scope.clone(), "historical-object".into()).await.unwrap().unwrap();
        assert_eq!(read.revision.envelope, envelope);
        let path = root.join("program").join(reference.sha256);
        tokio::fs::write(&path, b"corrupt source").await.unwrap();
        assert!(matches!(ObservationStore::get(store.as_ref(), scope.clone(), "historical-object".into()).await, Err(PortError::Unavailable)));
        tokio::fs::remove_file(path).await.unwrap();
        assert!(matches!(ObservationStore::get(store.as_ref(), scope, "historical-object".into()).await, Err(PortError::Unavailable)));
        let foreign = ObservationScope { principal: "foreign".into(), source_session: None };
        assert!(ObservationStore::get(store.as_ref(), foreign, "historical-object".into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn script_sqlite_actual_additive_migration_cas_and_restart_contract() {
        let temporary = std::env::temp_dir();
        let preferred = temporary.join("opencode");
        let base = if preferred.is_dir() {
            preferred
        } else {
            temporary
        };
        let path = base.join(format!("script-s2-{}.sqlite", uuid::Uuid::new_v4()));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .unwrap();
        for migration in sqlx::migrate!("./migrations/sqlite")
            .iter()
            .filter(|m| m.version < 9)
        {
            pool.execute(migration.sql.as_ref()).await.unwrap();
        }
        pool.execute("CREATE TABLE unrelated_s2_sentinel (value TEXT NOT NULL); INSERT INTO unrelated_s2_sentinel VALUES ('preserved')").await.unwrap();
        pool.execute(OLD_HISTORY).await.unwrap();
        pool.execute(include_str!(
            "../../migrations/sqlite/0009_script_invocations.sql"
        ))
        .await
        .unwrap();
        let store = SqlInvocationStore::new(PersistencePool::Sqlite(pool.clone()));
        pool.execute(include_str!("../../migrations/sqlite/0010_script_text_results.sql")).await.unwrap();
        pool.execute(include_str!("../../migrations/sqlite/0016_routine_object_references.sql")).await.unwrap();
        contract(&store).await;
        let legacy: (String,) = sqlx::query_as(
            "SELECT status FROM routine_invocations WHERE invocation_id='legacy-sentinel'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(legacy.0, "admitted");
        let sentinel: (String,) = sqlx::query_as("SELECT value FROM unrelated_s2_sentinel")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sentinel.0, "preserved");
        let revisions: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM script_program_revisions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(revisions.0, 1);
        let reference: (String, i64) = sqlx::query_as("SELECT object_sha256,object_bytes FROM script_program_revisions")
            .fetch_one(&pool)
            .await
            .unwrap();
        let object = crate::routine_objects::ObjectRef::new(crate::routine_objects::Namespace::Program, reference.0, reference.1 as u64).unwrap();
        let body = store.objects.get(&object).await.unwrap();
        assert_eq!(body, admission("unused").revision.envelope.as_bytes());
        pool.close().await;
        std::fs::remove_file(path).unwrap();
    }

    /// Only run against a newly created disposable local PostgreSQL database.
    #[tokio::test]
    #[ignore = "requires SCRIPT_TEST_POSTGRES_URL for a private ephemeral script_s2_ephemeral database"]
    async fn script_postgres_actual_additive_migration_cas_and_restart_contract() {
        let url =
            std::env::var("SCRIPT_TEST_POSTGRES_URL").expect("explicit disposable database URL");
        assert!(
            url.starts_with("postgres://script_s2:")
                && url.contains("@127.0.0.1:")
                && url.ends_with("/script_s2_ephemeral"),
            "refusing non-fixture database"
        );
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .unwrap();
        // Fail if this is not a fresh database; never reset an existing target.
        pool.execute("CREATE SCHEMA waltr").await.unwrap();
        for migration in sqlx::migrate!("./migrations/postgresql")
            .iter()
            .filter(|m| m.version < 9)
        {
            pool.execute(migration.sql.as_ref()).await.unwrap();
        }
        pool.execute("CREATE TABLE public.unrelated_s2_sentinel (value TEXT NOT NULL); INSERT INTO public.unrelated_s2_sentinel VALUES ('preserved')").await.unwrap();
        pool.execute(
            OLD_HISTORY
                .replace("INTO routine_invocations", "INTO waltr.routine_invocations")
                .as_str(),
        )
        .await
        .unwrap();
        pool.execute(include_str!(
            "../../migrations/postgresql/0009_script_invocations.sql"
        ))
        .await
        .unwrap();
        pool.execute(include_str!("../../migrations/postgresql/0010_script_text_results.sql")).await.unwrap();
        pool.execute(include_str!("../../migrations/postgresql/0016_routine_object_references.sql")).await.unwrap();
        contract(&SqlInvocationStore::new(PersistencePool::PostgreSql(
            pool.clone(),
        )))
        .await;
        let legacy: (String,) = sqlx::query_as(
            "SELECT status FROM waltr.routine_invocations WHERE invocation_id='legacy-sentinel'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(legacy.0, "admitted");
        let sentinel: (String,) = sqlx::query_as("SELECT value FROM public.unrelated_s2_sentinel")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sentinel.0, "preserved");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM waltr.script_program_revisions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
        pool.close().await;
    }
}
