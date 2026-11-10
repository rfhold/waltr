use super::*;
use crate::{
    persistence::script_invocation::SqlInvocationStore,
    routine::{
        definition::{API_VERSION, revision_id},
        history::{Admission, Caller, Origin, Ownership, Revision, Status},
        ports::InvocationStore,
        text_results::{TextSourceFuture, process_credit},
    },
};
use sqlx::Executor;
use std::sync::atomic::{AtomicUsize, Ordering};

mod evidence;
mod objects;

struct BytesSource {
    bytes: Vec<u8>,
    offset: usize,
    chunk: usize,
    reads: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    failure_after: Option<usize>,
}
impl Drop for BytesSource {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl TextSource for BytesSource {
    fn read<'a>(&'a mut self, destination: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.failure_after.is_some_and(|after| self.offset >= after) {
                return Err(TextResultError::Unavailable);
            }
            assert!(destination.len() <= MAX_CHUNK_BYTES);
            let count = destination
                .len()
                .min(self.chunk)
                .min(self.bytes.len() - self.offset);
            destination[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
fn source(bytes: Vec<u8>, chunk: usize) -> BytesSource {
    BytesSource {
        bytes,
        offset: 0,
        chunk,
        reads: Arc::new(AtomicUsize::new(0)),
        dropped: Arc::new(AtomicUsize::new(0)),
        failure_after: None,
    }
}
fn scope(id: &str) -> InvocationTextScope {
    InvocationTextScope::new("owner".into(), id.into()).unwrap()
}
fn admission(id: &str) -> Admission {
    let envelope = r#"{"apiVersion":"waltr.routine/v1","kind":"Routine","name":"test","spec":{"source":"export default async function() {}"}}"#;
    let engine = "text-result-fixture;abi=1";
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
            principal: "owner".into(),
            origin: Origin::Manual,
            ownership: Ownership::Detached,
            correlation: "not-authority".into(),
        },
        entrypoint: "run".into(),
        snapshot: None,
        admitted_ms: 1,
    }
}
async fn running(pool: &PersistencePool, id: &str) {
    let history = SqlInvocationStore::new(pool.clone());
    history.admit(admission(id)).await.unwrap();
    history.running(id.into(), 2).await.unwrap();
}
async fn publish(
    store: &dyn TextResultStore,
    id: &str,
    name: &str,
    bytes: Vec<u8>,
    chunk: usize,
) -> Result<TextResultDescriptor, TextResultError> {
    store
        .publish(
            scope(id),
            name.into(),
            Some("helper-reference".into()),
            Box::new(source(bytes, chunk)),
            CancellationToken::new(),
        )
        .await
}
async fn quota_sum(pool: &PersistencePool, id: &str) -> (i64, i64) {
    backend!(pool, |pool, schema| {
        sqlx::query_as(&format!("SELECT encoded_bytes,result_count FROM {schema}script_text_result_usage WHERE invocation=$1"))
            .bind(id).fetch_one(pool).await.unwrap()
    })
}

async fn text_result_contract(pool: PersistencePool) {
    let store = SqlTextResultStore {
        pool: pool.clone(),
        objects: crate::routine_objects::test_objects(),
    };
    for id in [
        "ascii", "unicode", "escaped", "exact", "race", "same", "invalid", "count", "other",
    ] {
        running(&pool, id).await;
    }
    let ascii = publish(
        &store,
        "ascii",
        "notes",
        vec![b'x'; MAX_TEXT_BYTES],
        MAX_CHUNK_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(ascii.utf8_bytes, MAX_TEXT_BYTES as u64);
    assert_eq!(
        ascii.sha256,
        Sha256::digest(vec![b'x'; MAX_TEXT_BYTES])
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert!(serde_json::to_vec(&ascii).unwrap().len() < 1024);
    assert!(!format!("{ascii:?}").contains("helper-reference"));
    let text = "🦀".repeat(MAX_TEXT_BYTES / 4);
    let unicode = publish(&store, "unicode", "cleaned", text.as_bytes().to_vec(), 7)
        .await
        .unwrap();
    let mut roundtrip = Vec::new();
    let mut offset = 0;
    while offset < unicode.utf8_bytes {
        let chunk = store
            .read(
                scope("unicode"),
                unicode.id.clone(),
                offset,
                65_535,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(chunk.bytes().len() <= MAX_CHUNK_BYTES);
        roundtrip.extend_from_slice(chunk.bytes());
        offset += chunk.bytes().len() as u64;
    }
    assert_eq!(roundtrip, text.as_bytes());
    assert_eq!(String::from_utf8(roundtrip).unwrap(), text);
    assert_eq!(
        store
            .read(
                scope("unicode"),
                unicode.id.clone(),
                1,
                1,
                CancellationToken::new()
            )
            .await
            .unwrap()
            .bytes(),
        &[0x9f]
    );
    assert!(
        store
            .read(
                scope("unicode"),
                unicode.id.clone(),
                unicode.utf8_bytes,
                64,
                CancellationToken::new()
            )
            .await
            .unwrap()
            .bytes()
            .is_empty()
    );
    for (offset, max) in [
        (unicode.utf8_bytes + 1, 1),
        (u64::MAX, 1),
        (0, 0),
        (0, 65_537),
    ] {
        assert_eq!(
            store
                .read(
                    scope("unicode"),
                    unicode.id.clone(),
                    offset,
                    max,
                    CancellationToken::new()
                )
                .await
                .unwrap_err(),
            TextResultError::Invalid
        );
    }
    let escaped_bytes = vec![0; MAX_TEXT_BYTES];
    let escaped = publish(&store, "escaped", "deletion_audit", escaped_bytes, 33)
        .await
        .unwrap();
    let encoded = serde_json::to_vec(&serde_json::json!({
        "descriptor": escaped, "text": "\0".repeat(MAX_TEXT_BYTES)
    }))
    .unwrap();
    assert_eq!(encoded.len() as u64, escaped.encoded_charge);
    assert!(escaped.encoded_charge > (6 * MAX_TEXT_BYTES) as u64);
    assert_eq!(
        publish(
            &store,
            "escaped",
            "over",
            vec![0; MAX_TEXT_BYTES],
            MAX_CHUNK_BYTES
        )
        .await
        .unwrap_err(),
        TextResultError::Quota
    );
    assert_eq!(store.list(scope("escaped")).await.unwrap().len(), 1);

    let mut total = 0u64;
    for index in 0..7 {
        total += publish(
            &store,
            "exact",
            &format!("item{index}"),
            vec![b'x'; MAX_TEXT_BYTES],
            MAX_CHUNK_BYTES,
        )
        .await
        .unwrap()
        .encoded_charge;
    }
    let target = MAX_ENCODED_BYTES as u64 - total;
    let mut low = 0;
    let mut high = MAX_TEXT_BYTES;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let candidate = "x".repeat(mid);
        let mut descriptor = ascii.clone();
        descriptor.name = "last".into();
        descriptor.utf8_bytes = mid as u64;
        envelope_charge(&mut descriptor, &candidate).unwrap();
        if descriptor.encoded_charge <= target {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let last = publish(&store, "exact", "last", vec![b'x'; low], MAX_CHUNK_BYTES)
        .await
        .unwrap();
    assert_eq!(total + last.encoded_charge, MAX_ENCODED_BYTES as u64);
    assert_eq!(
        quota_sum(&pool, "exact").await,
        (MAX_ENCODED_BYTES as i64, 8)
    );
    assert_eq!(
        publish(&store, "exact", "extra", vec![], 1)
            .await
            .unwrap_err(),
        TextResultError::Quota
    );

    publish(
        &store,
        "race",
        "base",
        vec![0; MAX_TEXT_BYTES],
        MAX_CHUNK_BYTES,
    )
    .await
    .unwrap();
    let (left, right) = tokio::join!(
        publish(
            &store,
            "race",
            "left",
            vec![b'x'; MAX_TEXT_BYTES],
            MAX_CHUNK_BYTES
        ),
        publish(
            &store,
            "race",
            "right",
            vec![b'x'; MAX_TEXT_BYTES],
            MAX_CHUNK_BYTES
        )
    );
    assert_ne!(
        left.is_ok(),
        right.is_ok(),
        "aggregate CAS admits exactly one"
    );
    assert_eq!(left.err().or(right.err()).unwrap(), TextResultError::Quota);
    let (left, right) = tokio::join!(
        publish(&store, "same", "notes", b"first".to_vec(), 2),
        publish(&store, "same", "notes", b"second".to_vec(), 2)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    assert_eq!(
        left.as_ref().err().or(right.as_ref().err()).unwrap(),
        &TextResultError::Conflict
    );
    let winner = left.or(right).unwrap();
    assert_eq!(
        store.list(scope("same")).await.unwrap(),
        vec![winner.clone()]
    );
    assert_eq!(
        publish(&store, "same", "notes", b"replacement".to_vec(), 2)
            .await
            .unwrap_err(),
        TextResultError::Conflict
    );
    assert_eq!(store.list(scope("same")).await.unwrap(), vec![winner]);

    for index in 0..MAX_RESULTS {
        publish(&store, "count", &format!("result{index}"), vec![], 1)
            .await
            .unwrap();
    }
    assert_eq!(store.list(scope("count")).await.unwrap().len(), MAX_RESULTS);
    assert_eq!(
        publish(&store, "count", "extra", vec![], 1)
            .await
            .unwrap_err(),
        TextResultError::Quota
    );
    for (bytes, error) in [
        (vec![b'x'; MAX_TEXT_BYTES + 1], TextResultError::Quota),
        (vec![0xf0, 0x9f, 0xa6], TextResultError::Invalid),
        (vec![b'a', 0xff], TextResultError::Invalid),
    ] {
        assert_eq!(
            publish(&store, "invalid", "invalid", bytes, 1)
                .await
                .unwrap_err(),
            error
        );
        assert!(store.list(scope("invalid")).await.unwrap().is_empty());
    }
    assert_eq!(
        store
            .publish(
                scope("invalid"),
                "bad_length".into(),
                None,
                Box::new(OverreportedSource),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        TextResultError::Invalid
    );
    let mut failed = source(b"partial text".to_vec(), 3);
    failed.failure_after = Some(3);
    let dropped = failed.dropped.clone();
    assert_eq!(
        store
            .publish(
                scope("invalid"),
                "failed".into(),
                None,
                Box::new(failed),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        TextResultError::Unavailable
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        store
            .publish(
                scope("invalid"),
                "cancelled".into(),
                None,
                Box::new(source(b"x".to_vec(), 1)),
                cancel
            )
            .await
            .unwrap_err(),
        TextResultError::Cancelled
    );
    assert!(store.list(scope("invalid")).await.unwrap().is_empty());
    let notification = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let task = tokio::spawn(store.publish(
        scope("invalid"),
        "pending".into(),
        None,
        Box::new(PendingSource {
            notification: notification.clone(),
            dropped: dropped.clone(),
        }),
        cancel.clone(),
    ));
    notification.notified().await;
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err(),
        TextResultError::Cancelled
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(store.list(scope("invalid")).await.unwrap().is_empty());
    for name in ["", "bad/name", &"x".repeat(65), "nonasciié"] {
        assert_eq!(
            publish(&store, "invalid", name, vec![], 1)
                .await
                .unwrap_err(),
            TextResultError::Invalid
        );
    }
    let mut huge_capacity_name = String::with_capacity(16 * 1024 * 1024);
    huge_capacity_name.push_str("notes");
    let input = source(vec![b'x'], 1);
    let reads = input.reads.clone();
    assert_eq!(
        store
            .publish(
                scope("invalid"),
                huge_capacity_name,
                None,
                Box::new(input),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        TextResultError::Invalid
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    for unauthorized in [
        InvocationTextScope::new("foreign".into(), "ascii".into()).unwrap(),
        scope("missing"),
    ] {
        let input = source(b"must not read".to_vec(), 1);
        let reads = input.reads.clone();
        let dropped = input.dropped.clone();
        assert_eq!(
            store
                .publish(
                    unauthorized.clone(),
                    "notes".into(),
                    None,
                    Box::new(input),
                    CancellationToken::new()
                )
                .await
                .unwrap_err(),
            TextResultError::Denied
        );
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.list(unauthorized.clone()).await.unwrap_err(),
            TextResultError::Denied
        );
        assert_eq!(
            store
                .read(
                    unauthorized,
                    ascii.id.clone(),
                    0,
                    1,
                    CancellationToken::new()
                )
                .await
                .unwrap_err(),
            TextResultError::Denied
        );
    }
    for (scope, id) in [
        (scope("other"), ascii.id.clone()),
        (scope("ascii"), uuid::Uuid::new_v4().to_string()),
    ] {
        assert_eq!(
            store
                .read(scope, id, 0, 1, CancellationToken::new())
                .await
                .unwrap_err(),
            TextResultError::Denied
        );
    }
    SqlInvocationStore::new(pool.clone())
        .terminal("ascii".into(), Status::Failed, None, 3)
        .await
        .unwrap();
    assert_eq!(
        store.list(scope("ascii")).await.unwrap(),
        vec![ascii.clone()]
    );
    assert_eq!(
        store
            .read(scope("ascii"), ascii.id, 0, 4, CancellationToken::new())
            .await
            .unwrap()
            .bytes(),
        b"xxxx"
    );
    let input = source(b"cannot inject after terminal".to_vec(), 1);
    let reads = input.reads.clone();
    assert_eq!(
        store
            .publish(
                scope("ascii"),
                "late".into(),
                None,
                Box::new(input),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        TextResultError::Conflict
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let all_credit = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        process_credit().acquire_many_owned(64 * 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    let cancel = CancellationToken::new();
    let input = source(vec![b'x'], 1);
    let reads = input.reads.clone();
    let task = tokio::spawn(store.publish(
        scope("invalid"),
        "waiting_credit".into(),
        None,
        Box::new(input),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err(),
        TextResultError::Cancelled
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    drop(all_credit);
}

struct OverreportedSource;
impl TextSource for OverreportedSource {
    fn read<'a>(&'a mut self, _: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async { Ok(usize::MAX) })
    }
}
struct PendingSource {
    notification: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicUsize>,
}
impl Drop for PendingSource {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl TextSource for PendingSource {
    fn read<'a>(&'a mut self, _: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async move {
            self.notification.notify_one();
            std::future::pending().await
        })
    }
}

const SENTINELS: &str = "
INSERT INTO outbound_oauth_credentials (target_key,credential_json,revision,updated_at_ms) VALUES ('text-result-sentinel','opaque-sentinel',7,1);
";
async fn seed_prior_invocation(pool: &PersistencePool) -> String {
    crate::persistence::test_history::seed(pool.clone(), "text-result-preserved").await
}

async fn prior_invocation_snapshot(pool: &PersistencePool) -> String {
    crate::persistence::test_history::snapshot(
        &SqlInvocationStore::new(pool.clone()),
        "text-result-preserved",
    )
    .await
}

async fn verify_sentinels(pool: &PersistencePool) {
    assert_eq!(
        SqlInvocationStore::new(pool.clone())
            .get("text-result-prior".into())
            .await
            .unwrap()
            .unwrap()
            .caller
            .principal,
        "owner"
    );
    backend!(pool, |pool, schema| {
        let oauth: (String, i64) = sqlx::query_as(&format!("SELECT credential_json,CAST(revision AS BIGINT) FROM {schema}outbound_oauth_credentials WHERE target_key='text-result-sentinel'"))
            .fetch_one(pool).await.unwrap();
        assert_eq!(oauth, ("opaque-sentinel".into(), 7));
    })
}

#[tokio::test]
async fn text_result_sqlite_additive_contract() {
    let path = std::env::temp_dir()
        .join("opencode")
        .join(format!("text-result-{}.sqlite", uuid::Uuid::new_v4()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    drop(file);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true)
                .busy_timeout(std::time::Duration::from_secs(5)),
        )
        .await
        .unwrap();
    for migration in sqlx::migrate!("./migrations/sqlite")
        .iter()
        .filter(|m| m.version < 10)
    {
        pool.execute(migration.sql.as_ref()).await.unwrap();
    }
    pool.execute(SENTINELS).await.unwrap();
    let backend = PersistencePool::Sqlite(pool.clone());
    pool.execute(include_str!(
        "../../../migrations/sqlite/0010_script_text_results.sql"
    ))
    .await
    .unwrap();
    pool.execute(include_str!(
        "../../../migrations/sqlite/0016_routine_object_references.sql"
    ))
    .await
    .unwrap();
    running(&backend, "text-result-prior").await;
    let prior_snapshot = seed_prior_invocation(&backend).await;
    text_result_contract(backend.clone()).await;
    verify_sentinels(&backend).await;
    assert_eq!(
        prior_invocation_snapshot(&backend).await,
        prior_snapshot,
        "actual prior JS invocation remains unchanged after 0010 and text publications"
    );
    pool.close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
#[ignore = "requires fresh private loopback SCRIPT_TEXT_RESULT_POSTGRES_URL/script_text_result_ephemeral database"]
async fn text_result_postgres_additive_contract() {
    let raw =
        std::env::var("SCRIPT_TEXT_RESULT_POSTGRES_URL").expect("explicit ephemeral fixture URL");
    let url = url::Url::parse(&raw).expect("fixture URL");
    assert!(matches!(url.scheme(), "postgres" | "postgresql"));
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert!(url.port().is_some());
    assert_eq!(url.username(), "script_text_result");
    assert_eq!(url.path(), "/script_text_result_ephemeral");
    assert!(
        url.query().is_none() && url.fragment().is_none(),
        "no target overrides"
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&raw)
        .await
        .unwrap();
    pool.execute("CREATE SCHEMA waltr")
        .await
        .expect("must fail if schema already exists; never reset a target");
    for migration in sqlx::migrate!("./migrations/postgresql")
        .iter()
        .filter(|m| m.version < 10)
    {
        pool.execute(migration.sql.as_ref()).await.unwrap();
    }
    pool.execute(
        SENTINELS
            .replace(
                "INTO outbound_oauth_credentials",
                "INTO waltr.outbound_oauth_credentials",
            )
            .as_str(),
    )
    .await
    .unwrap();
    let backend = PersistencePool::PostgreSql(pool.clone());
    pool.execute(include_str!(
        "../../../migrations/postgresql/0010_script_text_results.sql"
    ))
    .await
    .unwrap();
    pool.execute(include_str!(
        "../../../migrations/postgresql/0016_routine_object_references.sql"
    ))
    .await
    .unwrap();
    running(&backend, "text-result-prior").await;
    let prior_snapshot = seed_prior_invocation(&backend).await;
    text_result_contract(backend.clone()).await;
    verify_sentinels(&backend).await;
    assert_eq!(
        prior_invocation_snapshot(&backend).await,
        prior_snapshot,
        "actual prior JS invocation remains unchanged after 0010 and text publications"
    );
    pool.close().await;
}
