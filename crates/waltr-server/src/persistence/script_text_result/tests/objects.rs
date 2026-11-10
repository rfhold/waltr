use super::*;
use crate::routine_objects::{ImmutableObjects, Namespace, ObjectError, ObjectFuture, ObjectRef};

struct FaultObjects {
    inner: Arc<dyn ImmutableObjects>,
    fail: bool,
    cancel: Option<CancellationToken>,
    terminal: Option<(sqlx::SqlitePool, String)>,
    puts: Arc<AtomicUsize>,
}
impl ImmutableObjects for FaultObjects {
    fn put<'a>(&'a self, reference: &'a ObjectRef, bytes: &'a [u8]) -> ObjectFuture<'a, ()> {
        Box::pin(async move {
            self.puts.fetch_add(1, Ordering::SeqCst);
            if self.fail { return Err(ObjectError::Unavailable); }
            self.inner.put(reference, bytes).await?;
            if let Some(cancel) = &self.cancel { cancel.cancel(); }
            if let Some((pool, id)) = &self.terminal {
                sqlx::query("UPDATE script_invocations SET status='failed' WHERE id=$1").bind(id).execute(pool).await.map_err(|_| ObjectError::Unavailable)?;
            }
            Ok(())
        })
    }
    fn get<'a>(&'a self, reference: &'a ObjectRef) -> ObjectFuture<'a, Vec<u8>> { self.inner.get(reference) }
}

#[tokio::test]
async fn object_write_failure_cancel_and_terminal_race_never_publish_or_charge() {
    for phase in ["write-failure", "cancel-after-write", "terminal-after-write", "sql-insert-failure"] {
        let persistence = Persistence::initialize(super::super::super::DatabaseConfig::for_tests().unwrap()).await.unwrap();
        let pool = persistence.pool.clone();
        running(&pool, phase).await;
        let cancellation = CancellationToken::new();
        let puts = Arc::new(AtomicUsize::new(0));
        let objects = Arc::new(FaultObjects {
            inner: crate::routine_objects::test_objects(), fail: phase == "write-failure",
            cancel: (phase == "cancel-after-write").then(|| cancellation.clone()),
            terminal: (phase == "terminal-after-write").then(|| (persistence.sqlite_pool().unwrap().clone(), phase.into())),
            puts: puts.clone(),
        });
        let store = SqlTextResultStore { pool: pool.clone(), objects };
        if phase == "sql-insert-failure" {
            sqlx::raw_sql("CREATE TRIGGER reject_object_publication BEFORE INSERT ON script_text_results BEGIN SELECT RAISE(ABORT,'injected'); END;").execute(persistence.sqlite_pool().unwrap()).await.unwrap();
        }
        let result = store.publish(scope(phase), "notes".into(), None, Box::new(source(b"body".to_vec(), 4)), cancellation).await;
        assert_eq!(result, Err(match phase { "cancel-after-write" => TextResultError::Cancelled, "terminal-after-write" => TextResultError::Conflict, _ => TextResultError::Unavailable }));
        assert_eq!(puts.load(Ordering::SeqCst), 1);
        assert!(store.list(scope(phase)).await.unwrap().is_empty());
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM script_text_result_usage").fetch_one(persistence.sqlite_pool().unwrap()).await.unwrap();
        assert_eq!(count, 0);
    }
}

#[tokio::test]
async fn authorized_text_read_rejects_missing_corrupt_objects_and_foreign_principal() {
    let persistence = Persistence::initialize(super::super::super::DatabaseConfig::for_tests().unwrap()).await.unwrap();
    let pool = persistence.pool.clone();
    running(&pool, "integrity").await;
    let root = crate::test_support::temp_root().join("payloads");
    let store = SqlTextResultStore { pool, objects: Arc::new(crate::routine_objects::FilesystemObjects::new(root.clone()).unwrap()) };
    let descriptor = publish(&store, "integrity", "notes", "exact é".as_bytes().to_vec(), 2).await.unwrap();
    let object = ObjectRef::new(Namespace::Text, descriptor.sha256.clone(), descriptor.utf8_bytes).unwrap();
    let foreign = InvocationTextScope::new("foreign".into(), "integrity".into()).unwrap();
    assert!(matches!(store.read(foreign, descriptor.id.clone(), 0, 64, CancellationToken::new()).await, Err(TextResultError::Denied)));
    let path = root.join("text").join(&object.sha256);
    tokio::fs::write(&path, b"corrupt").await.unwrap();
    assert!(matches!(store.read(scope("integrity"), descriptor.id.clone(), 0, 64, CancellationToken::new()).await, Err(TextResultError::Unavailable)));
    tokio::fs::remove_file(&path).await.unwrap();
    assert!(matches!(store.read(scope("integrity"), descriptor.id, 0, 64, CancellationToken::new()).await, Err(TextResultError::Unavailable)));
}

#[tokio::test]
async fn lost_sql_ack_is_uncertain_with_one_durable_object_and_no_automatic_republication() {
    use crate::persistence::script_text_result::evidence_hooks::{Guard, Stage};
    let persistence = Persistence::initialize(super::super::super::DatabaseConfig::for_tests().unwrap()).await.unwrap();
    let pool = persistence.pool.clone();
    let id = "object-lost-ack";
    running(&pool, id).await;
    let puts = Arc::new(AtomicUsize::new(0));
    let store = SqlTextResultStore { pool, objects: Arc::new(FaultObjects {
        inner: crate::routine_objects::test_objects(), fail: false, cancel: None, terminal: None, puts: puts.clone(),
    }) };
    let guard = Guard::install(id, Stage::Committed, true);
    let future = store.publish(scope(id), "notes".into(), None, Box::new(source(b"one business result".to_vec(), 8)), CancellationToken::new());
    let operation = tokio::spawn(future);
    tokio::time::timeout(std::time::Duration::from_secs(10), guard.reached()).await.unwrap();
    guard.release();
    assert_eq!(operation.await.unwrap(), Err(TextResultError::Uncertain));
    assert_eq!(puts.load(Ordering::SeqCst), 1);
    let descriptors = store.list(scope(id)).await.unwrap();
    assert_eq!(descriptors.len(), 1);
    let chunk = store.read(scope(id), descriptors[0].id.clone(), 0, 64, CancellationToken::new()).await.unwrap();
    assert_eq!(chunk.bytes(), b"one business result");
    assert_eq!(quota_sum(&store.pool, id).await, (descriptors[0].encoded_charge as i64, 1));
}
