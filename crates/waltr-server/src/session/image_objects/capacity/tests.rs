use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use waltr_agent::MessageImageFormat;

use super::*;
use crate::session::image_objects::SessionImageObjects;

const OWNER: &str = "https://issuer.test/\0subject";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage";

#[tokio::test]
async fn filesystem_charges_concurrent_independent_stores_and_reopens_at_cap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let store = SessionImageObjects::filesystem_with_limit(&root, 4 * PNG.len() as u64).unwrap();
    let len = PNG.len() as u64;
    let descriptor = store
        .reserve(OWNER, len)
        .await
        .unwrap()
        .put(MessageImageFormat::Png, PNG)
        .await
        .unwrap();

    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let separate = SessionImageObjects::filesystem_with_limit(&root, 4 * len).unwrap();
        workers.spawn(async move {
            separate
                .reserve(OWNER, len)
                .await?
                .put(MessageImageFormat::Png, PNG)
                .await
        });
    }
    let mut accepted = 0;
    while let Some(done) = workers.join_next().await {
        match done.unwrap() {
            Ok(saved) => {
                assert_eq!(saved, descriptor);
                accepted += 1;
            }
            Err(ImageObjectError::Capacity) => {}
            Err(error) => panic!("unexpected result: {error:?}"),
        }
    }
    assert_eq!(accepted, 3);

    let reopened = SessionImageObjects::filesystem_with_limit(&root, 4 * len).unwrap();
    assert_eq!(
        reopened.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Capacity)
    );
    assert_eq!(reopened.get(descriptor).await.unwrap(), PNG);
    assert_eq!(
        reopened.reserve(OWNER, 0).await.err(),
        Some(ImageObjectError::Invalid)
    );
    assert!(
        reopened
            .reserve(&OWNER.to_uppercase(), 4 * len)
            .await
            .is_ok()
    );
    assert!(
        SessionImageObjects::filesystem_with_limit(&temp.path().join("other-sessions"), 4 * len)
            .unwrap()
            .reserve(OWNER, 4 * len)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn filesystem_charges_whole_batch_without_refund_and_rejects_corrupt_counter() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let len = PNG.len() as u64;
    let store = SessionImageObjects::filesystem_with_limit(&root, 2 * len).unwrap();
    {
        let mut writer = store.reserve(OWNER, 2 * len).await.unwrap();
        writer.put(MessageImageFormat::Png, PNG).await.unwrap();
    }
    assert_eq!(
        store.reserve(OWNER, len).await.err(),
        Some(ImageObjectError::Capacity)
    );

    let namespace = temp.path().join("sessions.waltr-message-objects");
    let counter = namespace.join(NAMESPACE).join(owner_key(OWNER));
    fs::write(counter, [1; COUNTER_BYTES + 1]).unwrap();
    assert_eq!(
        store.reserve(OWNER, len).await.err(),
        Some(ImageObjectError::Corrupt)
    );
}

#[derive(Default)]
struct FakeS3 {
    counter: Option<u64>,
    puts: usize,
    gets: usize,
    image_puts: usize,
    concurrent_create: bool,
    always_conflict: bool,
    reject_if_match: bool,
    fail_after_put: bool,
    fail_after_image_put: bool,
    committed_image: bool,
    ledger: BTreeSet<String>,
    ledger_puts: usize,
    list_page_size: Option<usize>,
    lists: usize,
}

type SharedFake = Arc<Mutex<FakeS3>>;

fn etag(counter: u64) -> String {
    format!("\"etag-{counter}\"")
}

async fn fake_get(
    State(state): State<SharedFake>,
    Path((_, key)): Path<(String, String)>,
) -> Response {
    let mut state = state.lock().unwrap();
    if key.starts_with("records.waltr-message-objects/image/") {
        assert!(state.committed_image);
        return Body::from(PNG).into_response();
    }
    assert!(key.starts_with(&format!("records.waltr-message-objects/{NAMESPACE}/")));
    state.gets += 1;
    match state.counter {
        Some(counter) => Response::builder()
            .header("etag", etag(counter))
            .body(Body::from(counter.to_be_bytes().to_vec()))
            .unwrap(),
        None => (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchKey</Code></Error>",
        )
            .into_response(),
    }
}

async fn fake_list(State(state): State<SharedFake>, RawQuery(query): RawQuery) -> Response {
    let query = query.unwrap();
    let params: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(query.as_bytes()).collect();
    assert_eq!(params.get("list-type").unwrap(), "2");
    let prefix: &str = params["prefix"].as_ref();
    let max_keys: usize = params["max-keys"].parse().unwrap();
    let offset = params
        .get("continuation-token")
        .map_or(0, |token| token.parse().unwrap());
    let mut state = state.lock().unwrap();
    state.lists += 1;
    let count = state
        .ledger
        .iter()
        .filter(|key| key.starts_with(prefix))
        .count();
    let entries: Vec<_> = state
        .ledger
        .iter()
        .filter(|key| key.starts_with(prefix))
        .skip(offset)
        .take(max_keys.min(state.list_page_size.unwrap_or(max_keys)))
        .collect();
    let next = offset + entries.len();
    let truncated = next < count;
    let mut xml = format!(
        "<ListBucketResult><IsTruncated>{truncated}</IsTruncated><KeyCount>{}</KeyCount>",
        entries.len(),
    );
    if truncated {
        write!(
            &mut xml,
            "<NextContinuationToken>{next}</NextContinuationToken>"
        )
        .unwrap();
    }
    for key in entries {
        write!(
            &mut xml,
            "<Contents><Key>{key}</Key><Size>0</Size></Contents>"
        )
        .unwrap();
    }
    xml.push_str("</ListBucketResult>");
    xml.into_response()
}

async fn fake_put(
    State(state): State<SharedFake>,
    Path((_, key)): Path<(String, String)>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    let ambiguous = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "<Error><Code>InternalError</Code></Error>",
        )
            .into_response()
    };
    if key.starts_with("records.waltr-message-objects/image/") {
        state.image_puts += 1;
        assert_eq!(headers.get("if-none-match").unwrap(), "*");
        assert_eq!(bytes.as_ref(), PNG);
        if state.committed_image {
            return (
                StatusCode::PRECONDITION_FAILED,
                "<Error><Code>PreconditionFailed</Code></Error>",
            )
                .into_response();
        }
        state.committed_image = true;
        return if state.fail_after_image_put {
            ambiguous()
        } else {
            StatusCode::OK.into_response()
        };
    }
    if key.contains("/ledger/") {
        assert_eq!(headers.get("if-none-match").unwrap(), "*");
        assert!(!headers.contains_key("if-match"));
        assert!(bytes.is_empty());
        state.ledger_puts += 1;
        assert!(state.ledger.insert(key));
        return if state.fail_after_put {
            ambiguous()
        } else {
            StatusCode::OK.into_response()
        };
    }
    assert_eq!(
        key,
        format!(
            "records.waltr-message-objects/{NAMESPACE}/{}",
            owner_key(OWNER)
        )
    );
    state.puts += 1;
    let next = u64::from_be_bytes(bytes.as_ref().try_into().unwrap());
    let conflict = || {
        (
            StatusCode::PRECONDITION_FAILED,
            "<Error><Code>PreconditionFailed</Code></Error>",
        )
            .into_response()
    };
    match state.counter {
        Some(prior) => {
            assert!(!headers.contains_key("if-none-match"));
            assert_eq!(
                headers.get("if-match").unwrap().to_str().unwrap(),
                etag(prior)
            );
            if state.reject_if_match {
                return (
                    StatusCode::NOT_IMPLEMENTED,
                    "<Error><Code>NotImplemented</Code></Error>",
                )
                    .into_response();
            }
        }
        None => {
            assert!(!headers.contains_key("if-match"));
            assert_eq!(headers.get("if-none-match").unwrap(), "*");
            if state.concurrent_create {
                state.counter = Some(1);
                return conflict();
            }
        }
    }
    if state.always_conflict {
        return conflict();
    }
    state.counter = Some(next);
    if state.fail_after_put {
        return ambiguous();
    }
    StatusCode::OK.into_response()
}

async fn fake_s3(
    state: SharedFake,
    limit: u64,
) -> (SessionImageObjects, tokio::task::JoinHandle<()>) {
    let (client, server) = fake_s3_client(state).await;
    (
        test_images(client, S3SessionWriteMode::ConditionalEtagCas, limit),
        server,
    )
}

fn test_images(
    client: aws_sdk_s3::Client,
    mode: S3SessionWriteMode,
    limit: u64,
) -> SessionImageObjects {
    SessionImageObjects::s3_with_limit(client, "bucket".into(), "records/".into(), mode, limit)
        .unwrap()
}

async fn fake_s3_client(state: SharedFake) -> (aws_sdk_s3::Client, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/{bucket}/{*key}", get(fake_get).put(fake_put))
        .route("/{bucket}", get(fake_list))
        .route("/{bucket}/", get(fake_list))
        .with_state(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = crate::session::storage::s3_client(
        endpoint,
        "test".into(),
        "test".into(),
        "test".into(),
        true,
    );
    (client, server)
}

#[tokio::test]
async fn s3_retries_only_conditional_conflicts_and_preserves_an_uncertain_charge() {
    let state = Arc::new(Mutex::new(FakeS3 {
        concurrent_create: true,
        ..FakeS3::default()
    }));
    let (images, server) = fake_s3(state.clone(), 5).await;
    assert!(images.reserve(OWNER, 3).await.is_ok());
    {
        let mut state = state.lock().unwrap();
        assert_eq!((state.gets, state.puts, state.counter), (2, 2, Some(4)));
        state.fail_after_put = true;
    }
    assert_eq!(
        images.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Unavailable)
    );
    assert_eq!(
        images.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Capacity)
    );
    {
        let state = state.lock().unwrap();
        assert_eq!((state.gets, state.puts, state.counter), (4, 3, Some(5)));
        assert_eq!(state.image_puts, 0);
    }
    server.abort();
}

#[tokio::test]
async fn s3_bounds_contention_retries() {
    let state = Arc::new(Mutex::new(FakeS3 {
        always_conflict: true,
        ..FakeS3::default()
    }));
    let (images, server) = fake_s3(state.clone(), 5).await;
    assert_eq!(
        images.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Unavailable)
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(
            (state.gets, state.puts, state.counter),
            (S3_ATTEMPTS, S3_ATTEMPTS, None)
        );
    }
    server.abort();
}

#[tokio::test]
async fn s3_requires_conditional_updates_even_when_creates_work() {
    let state = Arc::new(Mutex::new(FakeS3 {
        reject_if_match: true,
        ..FakeS3::default()
    }));
    let (images, server) = fake_s3(state.clone(), 5).await;
    assert!(images.reserve(OWNER, 1).await.is_ok());
    assert_eq!(
        images.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Unavailable)
    );
    {
        let state = state.lock().unwrap();
        assert_eq!((state.gets, state.puts, state.counter), (2, 2, Some(1)));
        assert_eq!(state.image_puts, 0);
    }
    server.abort();
}

#[tokio::test]
async fn s3_does_not_retry_ambiguous_image_publication() {
    let state = Arc::new(Mutex::new(FakeS3 {
        fail_after_image_put: true,
        ..FakeS3::default()
    }));
    let len = PNG.len() as u64;
    let (images, server) = fake_s3(state.clone(), 2 * len).await;
    let mut writer = images.reserve(OWNER, 2 * len).await.unwrap();
    assert_eq!(
        writer.put(MessageImageFormat::Png, PNG).await,
        Err(ImageObjectError::Unavailable)
    );
    assert_eq!(
        writer.put(MessageImageFormat::Png, PNG).await,
        Err(ImageObjectError::Invalid)
    );
    assert_eq!(
        images.reserve(OWNER, len).await.err(),
        Some(ImageObjectError::Capacity)
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(state.counter, Some(2 * len));
        assert!(state.committed_image);
        assert_eq!(state.image_puts, 1);
    }
    server.abort();
}

#[tokio::test]
async fn single_writer_publishes_twice_without_if_match_and_reopens_at_quota() {
    let state = Arc::new(Mutex::new(FakeS3 {
        reject_if_match: true,
        list_page_size: Some(1),
        ..FakeS3::default()
    }));
    let (client, server) = fake_s3_client(state.clone()).await;
    let open = || {
        test_images(
            client.clone(),
            S3SessionWriteMode::SingleWriter,
            2 * ledger::UNIT,
        )
    };
    let images = open();
    let second_handle = open();
    let third_handle = open();
    let bytes = PNG.len() as u64;
    let (first, second, third) = tokio::join!(
        images.reserve(OWNER, bytes),
        second_handle.reserve(OWNER, bytes),
        third_handle.reserve(OWNER, bytes),
    );
    let mut saved = Vec::new();
    for result in [first, second, third] {
        match result {
            Ok(mut writer) => saved.push(writer.put(MessageImageFormat::Png, PNG).await.unwrap()),
            Err(error) => assert_eq!(error, ImageObjectError::Capacity),
        }
    }
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0], saved[1]);
    drop(images);
    drop(second_handle);
    drop(third_handle);
    let reopened = open();
    assert_eq!(
        reopened.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Capacity)
    );
    assert_eq!(reopened.get(saved[0]).await.unwrap(), PNG);
    {
        let state = state.lock().unwrap();
        assert_eq!(
            (state.ledger.len(), state.ledger_puts, state.image_puts),
            (2, 2, 2)
        );
        assert_eq!((state.lists, state.puts), (6, 0));
    }
    server.abort();
}

#[tokio::test]
async fn single_writer_does_not_retry_or_publish_on_an_uncertain_ledger_put() {
    let state = Arc::new(Mutex::new(FakeS3 {
        reject_if_match: true,
        fail_after_put: true,
        ..FakeS3::default()
    }));
    let (client, server) = fake_s3_client(state.clone()).await;
    let images = test_images(client, S3SessionWriteMode::SingleWriter, ledger::UNIT);
    assert_eq!(
        images.reserve(OWNER, PNG.len() as u64).await.err(),
        Some(ImageObjectError::Unavailable)
    );
    assert_eq!(
        images.reserve(OWNER, PNG.len() as u64).await.err(),
        Some(ImageObjectError::Capacity)
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(
            (state.ledger.len(), state.ledger_puts, state.image_puts),
            (1, 1, 0)
        );
    }
    server.abort();
}

#[tokio::test]
async fn s3_mode_changes_do_not_restore_capacity() {
    let state = Arc::new(Mutex::new(FakeS3::default()));
    let (client, server) = fake_s3_client(state.clone()).await;
    let open = |mode| test_images(client.clone(), mode, 2 * ledger::UNIT);
    let cas = open(S3SessionWriteMode::ConditionalEtagCas);
    assert!(cas.reserve(OWNER, ledger::UNIT).await.is_ok());
    drop(cas);
    let single = open(S3SessionWriteMode::SingleWriter);
    assert!(single.reserve(OWNER, 1).await.is_ok());
    assert_eq!(
        single.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Capacity)
    );
    drop(single);
    let cas = open(S3SessionWriteMode::ConditionalEtagCas);
    assert_eq!(
        cas.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Capacity)
    );
    {
        let state = state.lock().unwrap();
        assert_eq!((state.ledger_puts, state.puts), (1, 1));
    }
    server.abort();
}

#[tokio::test]
async fn single_writer_rejects_malformed_history() {
    let invalid = format!(
        "records.waltr-message-objects/{NAMESPACE}/{}/ledger/not-a-reservation",
        owner_key(OWNER)
    );
    let state = Arc::new(Mutex::new(FakeS3 {
        ledger: BTreeSet::from([invalid]),
        ..FakeS3::default()
    }));
    let (client, server) = fake_s3_client(state.clone()).await;
    let images = test_images(client, S3SessionWriteMode::SingleWriter, ledger::UNIT);
    assert_eq!(
        images.reserve(OWNER, 1).await.err(),
        Some(ImageObjectError::Corrupt)
    );
    assert_eq!(state.lock().unwrap().ledger_puts, 0);
    server.abort();
}
