use super::super::evidence_hooks::{Guard, Stage};
use super::*;
use futures_util::FutureExt;
use std::{
    future::poll_fn,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    task::Poll,
    time::{Duration, Instant},
};

const TOTAL_CREDIT: u32 = 64 * 1024 * 1024;
const CHILD_TEST: &str =
    "persistence::script_text_result::tests::evidence::text_result_private_child";

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        // This directory was exclusively created by this test; never remove another path.
        for name in [
            "fixture.sqlite",
            "fixture.sqlite-wal",
            "fixture.sqlite-shm",
            "fixture.sqlite-journal",
            "child.log",
        ] {
            drop(std::fs::remove_file(self.0.join(name)));
        }
        drop(std::fs::remove_dir(&self.0));
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

#[test]
fn text_result_sqlite_watchdog_transaction_and_credit_evidence() {
    let directory = std::env::temp_dir()
        .join("opencode")
        .join(format!("text-result-evidence-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let fixture = Fixture(directory);
    let path = fixture.0.join("fixture.sqlite");
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    let log = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(fixture.0.join("child.log"))
        .unwrap();
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", CHILD_TEST, "--nocapture"])
            .env("TEXT_RESULT_PRIVATE_CHILD_FILE", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "private SQLite child watchdog expired; Drop kills and reaps"
        );
        // Watchdog polling only; SQL/source ordering uses gates, never elapsed time.
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut output = String::new();
    std::fs::File::open(fixture.0.join("child.log"))
        .unwrap()
        .take(32 * 1024)
        .read_to_string(&mut output)
        .unwrap();
    assert!(status.success(), "private child failed: {output}");
    assert!(
        output.contains("TEXT_RESULT_EVIDENCE_COMPLETE"),
        "child did not execute all cases: {output}"
    );
}

#[test]
#[ignore = "parent-created empty-file child fixture only; use the watchdog parent test"]
fn text_result_private_child() {
    let path = PathBuf::from(
        std::env::var_os("TEXT_RESULT_PRIVATE_CHILD_FILE").expect("watchdog parent required"),
    );
    let parent = path.parent().unwrap();
    assert_eq!(path.file_name().unwrap(), "fixture.sqlite");
    let directory = parent.file_name().unwrap().to_str().unwrap();
    let nonce = directory.strip_prefix("text-result-evidence-").unwrap();
    uuid::Uuid::parse_str(nonce).unwrap();
    assert_eq!(
        parent.parent().unwrap().canonicalize().unwrap(),
        std::env::temp_dir()
            .join("opencode")
            .canonicalize()
            .unwrap()
    );
    assert!(std::fs::symlink_metadata(parent).unwrap().is_dir());
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(
        metadata.is_file() && metadata.len() == 0,
        "never open an existing populated DB or symlink"
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let pool = connect(&path, 5, 5000).await;
            for migration in sqlx::migrate!("./migrations/sqlite").iter() {
                pool.execute(migration.sql.as_ref()).await.unwrap();
            }
            let backend = PersistencePool::Sqlite(pool.clone());
            let store = SqlTextResultStore {
                pool: backend.clone(),
                objects: crate::routine_objects::test_objects(),
            };
            locking(&path, &backend, &store).await;
            cancellation_and_ack(&backend, &store).await;
            insert_failure(&pool, &backend, &store).await;
            source_and_future_lifecycle(&backend, &store).await;
            reader_credit(&backend, &store).await;
            separate_pool_quota(&path, &backend, &store).await;
            assert_all_credit_free();
            pool.close().await;
        });
    println!("TEXT_RESULT_EVIDENCE_COMPLETE");
}

async fn connect(path: &Path, connections: u32, busy_ms: u64) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(connections)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(false)
                .foreign_keys(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_millis(busy_ms)),
        )
        .await
        .unwrap()
}

async fn reach(gate: &Guard, publishing: &mut TextFuture<TextResultDescriptor>) {
    tokio::select! {
        () = gate.reached() => {},
        result = publishing => panic!("publish finished before its SQL gate: {result:?}"),
    }
}
fn assert_all_credit_free() {
    let credit = process_credit()
        .try_acquire_many_owned(TOTAL_CREDIT)
        .expect("all process credit returned");
    assert_eq!(credit.num_permits(), TOTAL_CREDIT as usize);
}
async fn no_artifact(store: &SqlTextResultStore, id: &str) {
    assert!(store.list(scope(id)).await.unwrap().is_empty());
    backend!(&store.pool, |pool, schema| {
        let row: Option<(i64, i64)> = sqlx::query_as(&format!("SELECT encoded_bytes,result_count FROM {schema}script_text_result_usage WHERE invocation=$1"))
            .bind(id).fetch_optional(pool).await.unwrap();
        assert!(
            row.is_none_or(|row| row == (0, 0)),
            "quota mutation rolled back"
        );
    });
}

async fn locking(path: &Path, backend: &PersistencePool, store: &SqlTextResultStore) {
    let id = "evidence-publish-lock";
    running(backend, id).await;
    let witness = "evidence-terminal-lock-witness";
    running(backend, witness).await;
    let gate = Guard::install(id, Stage::Locked, false);
    let mut publishing = store.publish(
        scope(id),
        "notes".into(),
        None,
        Box::new(source(b"selected".to_vec(), 3)),
        CancellationToken::new(),
    );
    reach(&gate, &mut publishing).await;
    // A second real SQL connection cannot terminalize while publication holds its UPDATE lock.
    let probe = connect(path, 1, 0).await;
    let error = sqlx::query("UPDATE script_invocations SET status='failed' WHERE id=$1")
        .bind(id)
        .execute(&probe)
        .await
        .unwrap_err();
    assert!(
        error
            .as_database_error()
            .is_some_and(|error| error.code().as_deref() == Some("5")),
        "expected SQLite BUSY, got {error}"
    );
    probe.close().await;
    // Check out the connection and begin first, so Pending means the real terminal
    // lock statement was submitted, not merely a pool-acquire wait.
    let terminal_pool = connect(path, 1, 5000).await;
    let terminal_tx = terminal_pool.begin().await.unwrap();
    let submitted = Arc::new(tokio::sync::Notify::new());
    let notify = submitted.clone();
    let mut waiter = Box::pin(async move {
        let mut terminal_tx = terminal_tx;
        // SQLite serializes writers across rows. The witness remains running even
        // if the real terminalizer wins the writer race after publication commits.
        let mut lock_statement = Box::pin(sqlx::query("UPDATE script_invocations SET status=status WHERE id=$1 AND status IN ('admitted','running')")
            .bind(witness).execute(&mut *terminal_tx));
        assert!(poll_fn(|cx| Poll::Ready(lock_statement.as_mut().poll(cx).is_pending())).await);
        notify.notify_one();
        assert_eq!(lock_statement.await.unwrap().rows_affected(), 1);
        terminal_tx.rollback().await.unwrap();
    });
    tokio::select! {
        () = submitted.notified() => {},
        () = &mut waiter => panic!("terminal lock statement completed before publisher released its SQL lock"),
    }
    let history = SqlInvocationStore::new(backend.clone());
    let mut terminal = history.terminal(id.into(), Status::Failed, None, 3);
    assert!(poll_fn(|cx| Poll::Ready(terminal.as_mut().poll(cx).is_pending())).await);
    gate.release();
    let (published, terminalized, ()) = tokio::join!(publishing, terminal, waiter);
    assert!(published.is_ok());
    assert_eq!(terminalized.unwrap(), Status::Failed);
    assert_eq!(store.list(scope(id)).await.unwrap().len(), 1);
    drop(gate);
    terminal_pool.close().await;

    let id = "evidence-terminal-first";
    running(backend, id).await;
    assert_eq!(
        history
            .terminal(id.into(), Status::Failed, None, 4)
            .await
            .unwrap(),
        Status::Failed
    );
    let input = source(b"never requested".to_vec(), 4);
    let reads = input.reads.clone();
    let dropped = input.dropped.clone();
    assert_eq!(
        store
            .publish(
                scope(id),
                "notes".into(),
                None,
                Box::new(input),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        TextResultError::Conflict
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    no_artifact(store, id).await;
    println!(
        "locking: real BUSY probe; publish-before-terminal retained; terminal-before-authorize unread"
    );
}

async fn cancellation_and_ack(backend: &PersistencePool, store: &SqlTextResultStore) {
    for (id, stage, expected) in [
        (
            "evidence-cancel-before",
            Stage::BeforeCancelCheck,
            Err(TextResultError::Cancelled),
        ),
        ("evidence-cancel-after", Stage::PassedCancelCheck, Ok(())),
        (
            "evidence-lost-ack",
            Stage::Committed,
            Err(TextResultError::Uncertain),
        ),
    ] {
        running(backend, id).await;
        let gate = Guard::install(id, stage, stage == Stage::Committed);
        let token = CancellationToken::new();
        let input = source(b"recover exact selected text".to_vec(), 7);
        let reads = input.reads.clone();
        let mut publishing = store.publish(
            scope(id),
            "selected".into(),
            Some("helper".into()),
            Box::new(input),
            token.clone(),
        );
        reach(&gate, &mut publishing).await;
        let before = reads.load(Ordering::SeqCst);
        token.cancel();
        gate.release();
        assert_eq!(publishing.await.map(|_| ()), expected);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            before,
            "no source replay after commit/uncertainty"
        );
        drop(gate);
        if stage == Stage::BeforeCancelCheck {
            no_artifact(store, id).await;
        } else {
            let descriptors = store.list(scope(id)).await.unwrap();
            assert_eq!(descriptors.len(), 1);
            assert_eq!(descriptors[0].name, "selected");
            let chunk = store
                .read(
                    scope(id),
                    descriptors[0].id.clone(),
                    0,
                    MAX_CHUNK_BYTES as u32,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(chunk.bytes(), b"recover exact selected text");
            drop(chunk);
            assert_eq!(
                quota_sum(backend, id).await,
                (descriptors[0].encoded_charge as i64, 1)
            );
            assert_eq!(
                publish(store, id, "selected", b"cannot replace".to_vec(), 4)
                    .await
                    .unwrap_err(),
                TextResultError::Conflict
            );
        }
        assert_all_credit_free();
    }
    println!(
        "commit: pre-check cancellation rolled back; post-check cancellation committed; real commit lost ACK recovered once"
    );
}

async fn insert_failure(
    pool: &sqlx::SqlitePool,
    backend: &PersistencePool,
    store: &SqlTextResultStore,
) {
    let id = "evidence-insert-fault";
    running(backend, id).await;
    pool.execute("CREATE TRIGGER text_result_insert_fault BEFORE INSERT ON script_text_results WHEN NEW.name='constraint_failure' BEGIN SELECT RAISE(ABORT,'bounded test insert fault'); END").await.unwrap();
    let gate = Guard::install(id, Stage::Charged, false);
    let mut publishing = store.publish(
        scope(id),
        "constraint_failure".into(),
        None,
        Box::new(source(b"business".to_vec(), 4)),
        CancellationToken::new(),
    );
    reach(&gate, &mut publishing).await;
    gate.release();
    assert_eq!(publishing.await.unwrap_err(), TextResultError::Unavailable);
    drop(gate);
    no_artifact(store, id).await;
    pool.execute("DROP TRIGGER text_result_insert_fault")
        .await
        .unwrap();
    publish(
        store,
        id,
        "constraint_failure",
        b"fresh recovery".to_vec(),
        4,
    )
    .await
    .unwrap();
    assert_eq!(quota_sum(backend, id).await.1, 1);
    assert_all_credit_free();
    println!(
        "insert fault: actual SQLite ABORT after quota UPDATE rolled back; fresh recovery succeeded"
    );
}

struct PanicSource {
    dropped: Arc<AtomicUsize>,
}
impl Drop for PanicSource {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl TextSource for PanicSource {
    fn read<'a>(&'a mut self, destination: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async move {
            assert!(destination.len() <= MAX_CHUNK_BYTES);
            panic!("bounded test source panic")
        })
    }
}

async fn source_and_future_lifecycle(backend: &PersistencePool, store: &SqlTextResultStore) {
    let id = "evidence-source-cleanup";
    running(backend, id).await;
    let dropped = Arc::new(AtomicUsize::new(0));
    let operation = store.publish(
        scope(id),
        "notes".into(),
        None,
        Box::new(PanicSource {
            dropped: dropped.clone(),
        }),
        CancellationToken::new(),
    );
    assert!(
        std::panic::AssertUnwindSafe(operation)
            .catch_unwind()
            .await
            .is_err()
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_all_credit_free();
    no_artifact(store, id).await;

    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut operation = store.publish(
        scope(id),
        "notes".into(),
        None,
        Box::new(PendingSource {
            notification: entered.clone(),
            dropped: dropped.clone(),
        }),
        CancellationToken::new(),
    );
    tokio::select! {
        () = entered.notified() => {},
        result = &mut operation => panic!("pending source completed: {result:?}"),
    }
    drop(operation);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_all_credit_free();
    no_artifact(store, id).await;

    let mut failed = source(vec![b'a'; MAX_CHUNK_BYTES], MAX_CHUNK_BYTES);
    failed.failure_after = Some(MAX_CHUNK_BYTES);
    let dropped = failed.dropped.clone();
    assert_eq!(
        store
            .publish(
                scope(id),
                "notes".into(),
                None,
                Box::new(failed),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        TextResultError::Unavailable
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_all_credit_free();
    no_artifact(store, id).await;
    assert_eq!(
        publish(store, id, "notes", vec![0xe2, 0x82], 1)
            .await
            .unwrap_err(),
        TextResultError::Invalid
    );
    assert_all_credit_free();
    no_artifact(store, id).await;

    let gate = Guard::install(id, Stage::BeforeCancelCheck, false);
    let mut operation = store.publish(
        scope(id),
        "notes".into(),
        None,
        Box::new(source(b"uncommitted".to_vec(), 4)),
        CancellationToken::new(),
    );
    reach(&gate, &mut operation).await;
    drop(operation);
    drop(gate);
    assert_all_credit_free();
    no_artifact(store, id).await;
    publish(store, id, "notes", b"fresh after every fault".to_vec(), 4)
        .await
        .unwrap();
    assert_eq!(quota_sum(backend, id).await.1, 1);
    assert_all_credit_free();
    println!(
        "ownership: panic, pending-future Drop, failed source, incomplete UTF8, open-TX Drop returned all 64MiB; no async producer/join claimed"
    );
}

async fn reader_credit(backend: &PersistencePool, store: &SqlTextResultStore) {
    let id = "evidence-reader-credit";
    running(backend, id).await;
    let descriptor = publish(
        store,
        id,
        "notes",
        vec![b'x'; MAX_CHUNK_BYTES],
        MAX_CHUNK_BYTES,
    )
    .await
    .unwrap();
    let mut held = process_credit()
        .try_acquire_many_owned(TOTAL_CREDIT)
        .unwrap();
    drop(held.split(MAX_CHUNK_BYTES).unwrap());
    let chunk = store
        .read(
            scope(id),
            descriptor.id,
            0,
            MAX_CHUNK_BYTES as u32,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(chunk.bytes().len(), MAX_CHUNK_BYTES);
    assert_eq!(
        process_credit().available_permits(),
        0,
        "read retains its entire 64KiB checkout"
    );
    assert!(process_credit().try_acquire_owned().is_err());
    drop(chunk);
    let returned = process_credit()
        .try_acquire_many_owned(MAX_CHUNK_BYTES as u32)
        .unwrap();
    held.merge(returned);
    assert_eq!(held.num_permits(), TOTAL_CREDIT as usize);
    drop(held);
    assert_all_credit_free();
    println!("reader: exact 64KiB owned credit unavailable until chunk Drop, then fully reclaimed");
}

struct BarrierSource {
    inner: BytesSource,
    barrier: Arc<tokio::sync::Barrier>,
}
impl TextSource for BarrierSource {
    fn read<'a>(&'a mut self, destination: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async move {
            let count = self.inner.read(destination).await?;
            if count == 0 {
                self.barrier.wait().await;
            }
            Ok(count)
        })
    }
}
async fn separate_pool_quota(path: &Path, backend: &PersistencePool, first: &SqlTextResultStore) {
    let id = "evidence-separate-pools";
    running(backend, id).await;
    let second_pool = connect(path, 1, 5000).await;
    let second = SqlTextResultStore {
        pool: PersistencePool::Sqlite(second_pool.clone()),
        objects: first.objects.clone(),
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let make = || {
        Box::new(BarrierSource {
            inner: source(vec![0; MAX_TEXT_BYTES], MAX_CHUNK_BYTES),
            barrier: barrier.clone(),
        }) as Box<dyn TextSource>
    };
    let (a, b) = tokio::join!(
        first.publish(
            scope(id),
            "selected_a".into(),
            None,
            make(),
            CancellationToken::new()
        ),
        second.publish(
            scope(id),
            "selected_b".into(),
            None,
            make(),
            CancellationToken::new()
        ),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        a.as_ref().err().or(b.as_ref().err()),
        Some(&TextResultError::Quota)
    );
    let winner = a.or(b).unwrap();
    assert_eq!(
        quota_sum(backend, id).await,
        (winner.encoded_charge as i64, 1)
    );
    assert!(winner.encoded_charge <= MAX_ENCODED_BYTES as u64);
    assert_eq!(
        first.list(scope(id)).await.unwrap(),
        second.list(scope(id)).await.unwrap()
    );
    assert_all_credit_free();
    second_pool.close().await;
    println!(
        "quota CAS: synchronized source EOF across independent SQLite pools/connections; one 6MiB-escaped winner, one Quota, <=8MiB total"
    );
}
