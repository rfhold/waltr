//! Deterministic dedicated S3 session-storage tests.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Response, StatusCode, header::ETAG},
    routing::get,
};
use waltr_agent::{
    MetadataSetCondition, PendingTimelineEvent, PendingTimelineItemId, Revision, S3SessionStore,
    S3SessionWriteMode, SessionCreateRequest, SessionId, SessionRecord, SessionStore,
    SessionStoreError, TimelineCursor,
};
use serde_json::json;
use tokio::{sync::Notify, task::JoinHandle};

#[tokio::test]
async fn conditional_mode_stops_after_eight_precondition_failures() {
    let fake = FakeS3::start(true).await;
    let store = S3SessionStore::new(fake.client().await, "bucket", "sessions");
    let id = SessionId::new("bounded-conditional");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");
    let initial_puts = fake.state.put_count.load(Ordering::SeqCst);
    fake.state
        .conflicts
        .lock()
        .expect("conflict lock")
        .extend(std::iter::repeat_n(ConflictAction::Reject, 8));

    let error = store
        .set_metadata_value(&id, "key".to_owned(), json!("value"))
        .await
        .expect_err("persistent 412 must fail");

    assert_eq!(
        fake.state.put_count.load(Ordering::SeqCst) - initial_puts,
        8
    );
    let SessionStoreError::Backend { message } = error else {
        panic!("expected backend error");
    };
    assert!(message.contains(id.as_str()));
    assert!(message.contains("status 409 or 412"));
    assert!(message.contains("8 PUT attempts"));
    assert!(message.contains("may not support PutObject If-Match"));
    assert!(message.contains("contending"));
}

#[tokio::test]
async fn default_conditional_mode_sends_if_match() {
    let fake = FakeS3::start(true).await;
    let store = S3SessionStore::new(fake.client().await, "bucket", "sessions");
    let id = SessionId::new("default-conditional");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");

    store
        .set_metadata_value(&id, "key".to_owned(), json!("value"))
        .await
        .expect("conditional update");

    let if_matches = fake.state.if_matches.lock().expect("header lock");
    assert_eq!(if_matches.len(), 2);
    assert_eq!(if_matches[0], None);
    assert_eq!(if_matches[1].as_deref(), Some("\"etag-1\""));
    drop(if_matches);
}

#[tokio::test]
async fn conditional_mode_rejects_a_loaded_object_without_an_etag() {
    let fake = FakeS3::start(false).await;
    let store = S3SessionStore::new(fake.client().await, "bucket", "sessions");
    let id = SessionId::new("conditional-missing-etag");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");
    let initial_puts = fake.state.put_count.load(Ordering::SeqCst);

    let error = store
        .set_metadata_value(&id, "key".to_owned(), json!("value"))
        .await
        .expect_err("conditional mode requires an ETag");

    assert_eq!(fake.state.put_count.load(Ordering::SeqCst), initial_puts);
    assert!(
        matches!(error, SessionStoreError::Backend { message } if message.contains(id.as_str()) && message.contains("no ETag required for conditional writes"))
    );
}

#[tokio::test]
async fn conditional_revision_update_reports_stale_after_conflict_reload() {
    let fake = FakeS3::start(true).await;
    let store = S3SessionStore::new(fake.client().await, "bucket", "sessions");
    let id = SessionId::new("stale-after-reload");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");
    fake.state
        .conflicts
        .lock()
        .expect("conflict lock")
        .push_back(ConflictAction::AdvanceRevision);

    let error = store
        .append_journal(&id, Revision::ZERO, Vec::new())
        .await
        .expect_err("changed revision must be stale");

    assert_eq!(
        error,
        SessionStoreError::StaleRevision {
            id,
            expected: Revision::ZERO,
            actual: Revision::new(1),
        }
    );
}

#[tokio::test]
async fn conditional_metadata_retry_reapplies_update_to_latest_record() {
    let fake = FakeS3::start(true).await;
    let store = S3SessionStore::new(fake.client().await, "bucket", "sessions");
    let id = SessionId::new("metadata-reapply");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");
    store
        .append_timeline(
            &id,
            Revision::ZERO,
            vec![PendingTimelineEvent::assistant_text_delta(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "preserved",
            )],
        )
        .await
        .expect("seed timeline high water");
    fake.state
        .conflicts
        .lock()
        .expect("conflict lock")
        .push_back(ConflictAction::AddServerMetadata);

    let record = store
        .set_metadata_value(&id, "client".to_owned(), json!(true))
        .await
        .expect("metadata retry");

    assert_eq!(record.metadata.values.get("server"), Some(&json!(true)));
    assert_eq!(record.metadata.values.get("client"), Some(&json!(true)));
    assert_eq!(record.timeline_cursor_high_water(), TimelineCursor::new(1));
}

#[tokio::test]
async fn conditional_metadata_retries_http_409_conflict() {
    let fake = FakeS3::start(true).await;
    let store = S3SessionStore::new(fake.client().await, "bucket", "sessions");
    let id = SessionId::new("metadata-409-retry");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");
    let initial_puts = fake.state.put_count.load(Ordering::SeqCst);
    fake.state
        .conflicts
        .lock()
        .expect("conflict lock")
        .push_back(ConflictAction::Reject409);

    let outcome = store
        .set_metadata_value_conditionally(
            &id,
            "key".to_owned(),
            json!("value"),
            MetadataSetCondition::Absent,
        )
        .await
        .expect("409 conflict should retry");

    assert!(outcome.changed);
    assert_eq!(
        fake.state.put_count.load(Ordering::SeqCst) - initial_puts,
        2
    );
}

#[tokio::test]
async fn concurrent_absent_metadata_writes_have_one_winner() {
    let fake = FakeS3::start(true).await;
    let client = fake.client().await;
    let first = S3SessionStore::new(client.clone(), "bucket", "sessions");
    let second = S3SessionStore::new(client, "bucket", "sessions");
    let id = SessionId::new("metadata-single-winner");
    first
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");

    let (left, right) = tokio::join!(
        first.set_metadata_value_conditionally(
            &id,
            "winner".to_owned(),
            json!("left"),
            MetadataSetCondition::Absent,
        ),
        second.set_metadata_value_conditionally(
            &id,
            "winner".to_owned(),
            json!("right"),
            MetadataSetCondition::Absent,
        ),
    );

    assert_eq!(
        usize::from(left.expect("left writer").changed)
            + usize::from(right.expect("right writer").changed),
        1
    );
}

#[tokio::test]
async fn single_writer_accepts_missing_etag_and_puts_without_if_match() {
    let fake = FakeS3::start(false).await;
    let store = S3SessionStore::new_with_write_mode(
        fake.client().await,
        "bucket",
        "sessions",
        S3SessionWriteMode::SingleWriter,
    );
    let id = SessionId::new("single-writer-no-etag");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");
    let initial_puts = fake.state.put_count.load(Ordering::SeqCst);

    let record = store
        .set_metadata_value(&id, "key".to_owned(), json!("value"))
        .await
        .expect("single-writer update");

    assert_eq!(record.metadata.values.get("key"), Some(&json!("value")));
    assert_eq!(
        fake.state.put_count.load(Ordering::SeqCst) - initial_puts,
        1
    );
    assert!(
        fake.state
            .if_matches
            .lock()
            .expect("header lock")
            .iter()
            .all(Option::is_none)
    );
}

#[tokio::test]
async fn normalized_prefixes_share_session_lock_across_store_instances_and_clones() {
    let fake = FakeS3::start(false).await;
    let client = fake.client().await;
    let first = S3SessionStore::new_with_write_mode(
        client.clone(),
        "bucket",
        "sessions",
        S3SessionWriteMode::SingleWriter,
    );
    let cloned = first.clone();
    let independent = S3SessionStore::new_with_write_mode(
        client,
        "bucket",
        "sessions/",
        S3SessionWriteMode::SingleWriter,
    );
    let id = SessionId::new("shared-process-lock");
    first
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .expect("create session");
    let gate = Arc::new(PutGate::default());
    *fake.state.next_put_gate.lock().expect("gate lock") = Some(Arc::clone(&gate));

    let first_id = id.clone();
    let first_update = tokio::spawn(async move {
        cloned
            .set_metadata_value(&first_id, "first".to_owned(), json!(true))
            .await
    });
    gate.entered.notified().await;
    let second_id = id.clone();
    let second_started = Arc::new(Notify::new());
    let task_started = Arc::clone(&second_started);
    let second_update = tokio::spawn(async move {
        task_started.notify_one();
        independent
            .set_metadata_value(&second_id, "second".to_owned(), json!(true))
            .await
    });
    second_started.notified().await;
    tokio::time::timeout(Duration::from_millis(50), fake.state.wait_for_put_count(3))
        .await
        .expect_err("second writer reached S3 while the first held the per-session guard");
    assert_eq!(
        fake.state.put_count.load(Ordering::SeqCst),
        2,
        "second writer reached S3 while the first held the per-session guard"
    );
    gate.release.notify_one();
    first_update
        .await
        .expect("first task")
        .expect("first update");
    second_update
        .await
        .expect("second task")
        .expect("second update");

    let record = first.load(&id).await.expect("load merged record");
    assert_eq!(record.metadata.values.get("first"), Some(&json!(true)));
    assert_eq!(record.metadata.values.get("second"), Some(&json!(true)));
}

#[derive(Debug, Clone, Copy)]
enum ConflictAction {
    Reject,
    Reject409,
    AdvanceRevision,
    AddServerMetadata,
}

#[derive(Debug, Default)]
struct PutGate {
    entered: Notify,
    release: Notify,
}

#[derive(Debug)]
struct FakeS3State {
    object: Mutex<Option<Vec<u8>>>,
    emit_etag: AtomicBool,
    etag_version: AtomicUsize,
    put_count: AtomicUsize,
    if_matches: Mutex<Vec<Option<String>>>,
    conflicts: Mutex<VecDeque<ConflictAction>>,
    next_put_gate: Mutex<Option<Arc<PutGate>>>,
    put_entered: Notify,
}

impl FakeS3State {
    const fn new(emit_etag: bool) -> Self {
        Self {
            object: Mutex::new(None),
            emit_etag: AtomicBool::new(emit_etag),
            etag_version: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
            if_matches: Mutex::new(Vec::new()),
            conflicts: Mutex::new(VecDeque::new()),
            next_put_gate: Mutex::new(None),
            put_entered: Notify::const_new(),
        }
    }

    async fn wait_for_put_count(&self, expected: usize) {
        loop {
            let entered = self.put_entered.notified();
            if self.put_count.load(Ordering::SeqCst) >= expected {
                return;
            }
            entered.await;
        }
    }
}

#[derive(Debug)]
struct FakeS3 {
    endpoint: String,
    state: Arc<FakeS3State>,
    server: JoinHandle<()>,
}

impl FakeS3 {
    async fn start(emit_etag: bool) -> Self {
        let state = Arc::new(FakeS3State::new(emit_etag));
        let app = Router::new()
            .route(
                "/{bucket}/{*key}",
                get(get_object).put(put_object).delete(delete_object),
            )
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake S3");
        let endpoint = format!(
            "http://{}",
            listener.local_addr().expect("listener address")
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve fake S3");
        });
        Self {
            endpoint,
            state,
            server,
        }
    }

    async fn client(&self) -> aws_sdk_s3::Client {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .endpoint_url(&self.endpoint)
            .region(aws_sdk_s3::config::Region::new("test"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "access-key",
                "secret-key",
                None,
                None,
                "fake-s3",
            ))
            .load()
            .await;
        let config = aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(config)
    }
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn get_object(State(state): State<Arc<FakeS3State>>) -> Response<Body> {
    let object = state.object.lock().expect("object lock").clone();
    let Some(object) = object else {
        return s3_error(StatusCode::NOT_FOUND, "NoSuchKey");
    };
    let mut response = Response::new(Body::from(object));
    if state.emit_etag.load(Ordering::SeqCst) {
        let version = state.etag_version.load(Ordering::SeqCst);
        response.headers_mut().insert(
            ETAG,
            format!("\"etag-{version}\"")
                .parse()
                .expect("valid ETag header"),
        );
    }
    response
}

async fn put_object(
    State(state): State<Arc<FakeS3State>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    state.put_count.fetch_add(1, Ordering::SeqCst);
    state.put_entered.notify_waiters();
    let if_match = headers
        .get("if-match")
        .map(|value| value.to_str().expect("text If-Match").to_owned());
    state
        .if_matches
        .lock()
        .expect("header lock")
        .push(if_match.clone());
    let gate = state.next_put_gate.lock().expect("gate lock").take();
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.release.notified().await;
    }
    let conflict = state.conflicts.lock().expect("conflict lock").pop_front();
    if let Some(conflict) = conflict {
        let (status, code) = match conflict {
            ConflictAction::Reject => (StatusCode::PRECONDITION_FAILED, "PreconditionFailed"),
            ConflictAction::Reject409 => (StatusCode::CONFLICT, "ConditionalRequestConflict"),
            ConflictAction::AdvanceRevision => {
                let mut object = state.object.lock().expect("object lock");
                let mut record: SessionRecord =
                    serde_json::from_slice(object.as_deref().expect("existing object"))
                        .expect("session record");
                record.revision = record.revision.next();
                *object = Some(serde_json::to_vec_pretty(&record).expect("serialize record"));
                drop(object);
                state.etag_version.fetch_add(1, Ordering::SeqCst);
                (StatusCode::PRECONDITION_FAILED, "PreconditionFailed")
            }
            ConflictAction::AddServerMetadata => {
                let mut object = state.object.lock().expect("object lock");
                let mut record: SessionRecord =
                    serde_json::from_slice(object.as_deref().expect("existing object"))
                        .expect("session record");
                record.set_metadata_value("server".to_owned(), json!(true));
                *object = Some(serde_json::to_vec_pretty(&record).expect("serialize record"));
                drop(object);
                state.etag_version.fetch_add(1, Ordering::SeqCst);
                (StatusCode::PRECONDITION_FAILED, "PreconditionFailed")
            }
        };
        return s3_error(status, code);
    }
    if let Some(if_match) = if_match {
        let object_exists = state.object.lock().expect("object lock").is_some();
        let version = state.etag_version.load(Ordering::SeqCst);
        if !object_exists || if_match != format!("\"etag-{version}\"") {
            return s3_error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed");
        }
    }
    *state.object.lock().expect("object lock") = Some(body.to_vec());
    state.etag_version.fetch_add(1, Ordering::SeqCst);
    Response::new(Body::empty())
}

async fn delete_object(State(state): State<Arc<FakeS3State>>) -> Response<Body> {
    *state.object.lock().expect("object lock") = None;
    Response::new(Body::empty())
}

fn s3_error(status: StatusCode, code: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/xml")
        .body(Body::from(format!("<Error><Code>{code}</Code></Error>")))
        .expect("S3 error response")
}
