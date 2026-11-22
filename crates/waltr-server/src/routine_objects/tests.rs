use super::*;
use axum::{body::{Body, Bytes}, extract::{Path, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::get, Router};
use std::collections::HashMap;
use std::sync::Mutex;

#[tokio::test]
async fn filesystem_roundtrip_immutable_digest_missing_and_corruption() {
    let root = crate::test_support::temp_root().join("payloads");
    let store = FilesystemObjects::new(root.clone()).unwrap();
    let object = ObjectRef::for_bytes(Namespace::Program, b"exact source").unwrap();
    assert!(!format!("{object:?}").contains(&object.sha256));
    assert_eq!(store.get(&object).await, Err(ObjectError::Missing));
    store.put(&object, b"exact source").await.unwrap();
    store.put(&object, b"exact source").await.unwrap();
    assert_eq!(store.get(&object).await.unwrap(), b"exact source");
    assert_eq!(store.put(&object, b"other source").await, Err(ObjectError::Corrupt));
    tokio::fs::write(root.join(object.key()), b"tampered").await.unwrap();
    assert_eq!(store.get(&object).await, Err(ObjectError::Corrupt));
    assert_eq!(store.put(&object, b"exact source").await, Err(ObjectError::Corrupt));
    assert_eq!(tokio::fs::read(root.join(object.key())).await.unwrap(), b"tampered");
    assert!(ObjectRef::new(Namespace::Text, "../path".into(), 0).is_err());
    assert!(ObjectRef::for_bytes(Namespace::Program, &vec![0; 131073]).is_err());
}

#[tokio::test]
async fn image_pending_file_is_removed_on_cancellation_without_removing_object() {
    use std::{future::{Future as _, pending, poll_fn}, task::Poll};

    let root = tempfile::tempdir().unwrap();
    let temporary = root.path().join(".pending");
    let published = root.path().join("published");
    {
        let mut publish = std::pin::pin!(async {
            let (file, owner) = PendingFile::create(temporary.clone()).unwrap();
            drop(file);
            std::fs::hard_link(owner.path(), &published).unwrap();
            pending::<()>().await;
            owner.remove().await.unwrap();
        });
        poll_fn(|cx| {
            assert!(publish.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }).await;
        assert!(temporary.exists());
    }
    assert!(!temporary.exists());
    assert!(published.exists());
}

type FakeState = Arc<Mutex<HashMap<String, Vec<u8>>>>;
async fn fake_get(State(state): State<FakeState>, Path((_, key)): Path<(String, String)>) -> Response {
    match state.lock().unwrap().get(&key).cloned() {
        Some(bytes) => Response::builder()
            .header("etag", format!("\"{}\"", digest(&bytes)))
            .body(Body::from(bytes))
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code></Error>").into_response(),
    }
}
async fn fake_put(State(state): State<FakeState>, Path((_, key)): Path<(String, String)>, headers: HeaderMap, bytes: Bytes) -> Response {
    let mut stored = state.lock().unwrap();
    let matches = match headers.get("if-match") {
        Some(etag) => {
            assert!(!headers.contains_key("if-none-match"));
            stored
                .get(&key)
                .is_some_and(|prior| etag.as_bytes() == format!("\"{}\"", digest(prior)).as_bytes())
        }
        None => {
            assert_eq!(headers.get("if-none-match").unwrap(), "*");
            !stored.contains_key(&key)
        }
    };
    if !matches {
        return (StatusCode::PRECONDITION_FAILED, "<Error><Code>PreconditionFailed</Code></Error>").into_response();
    }
    stored.insert(key, bytes.to_vec());
    StatusCode::OK.into_response()
}

async fn fake_empty_list() -> &'static str {
    "<ListBucketResult><IsTruncated>false</IsTruncated><KeyCount>0</KeyCount></ListBucketResult>"
}

#[tokio::test]
async fn fake_s3_roundtrip_routine_and_image_objects() {
    let state: FakeState = Arc::default();
    let app = Router::new().route("/{bucket}/{*key}", get(fake_get).put(fake_put)).route("/{bucket}/", get(fake_empty_list)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let stop = tokio_util::sync::CancellationToken::new();
    let stopped = stop.clone();
    let server = tokio::spawn(async move { axum::serve(listener, app).with_graceful_shutdown(stopped.cancelled_owned()).await.unwrap(); });
    let client = crate::session::storage::s3_client(endpoint, "test".into(), "test".into(), "test".into(), true);
    let store = S3Objects::new(client.clone(), "bucket".into(), "test".into()).unwrap();
    let object = ObjectRef::for_bytes(Namespace::Text, "UTF-8 é".as_bytes()).unwrap();
    assert_eq!(store.get(&object).await, Err(ObjectError::Missing));
    store.put(&object, "UTF-8 é".as_bytes()).await.unwrap();
    store.put(&object, "UTF-8 é".as_bytes()).await.unwrap();
    assert_eq!(store.get(&object).await.unwrap(), "UTF-8 é".as_bytes());
    state.lock().unwrap().insert(store.key(&object), b"corrupt".to_vec());
    assert_eq!(store.get(&object).await, Err(ObjectError::Corrupt));
    assert_eq!(store.put(&object, "UTF-8 é".as_bytes()).await, Err(ObjectError::Corrupt));

    use crate::session::image_objects::SessionImageObjects;
    use tracing::instrument::WithSubscriber as _;
    use waltr_agent::{MessageImageFormat, S3SessionWriteMode};

    #[derive(Clone, Default)]
    struct Trace(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Trace {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let trace = Trace::default();
    let capture = trace.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || capture.clone())
        .finish();

    let png = b"\x89PNG\r\n\x1a\nsaved-image-payload";
    let (reopened, image) = async {
        tracing::trace!("trace capture enabled");
        let mode = S3SessionWriteMode::ConditionalEtagCas;
        let images = SessionImageObjects::s3(client.clone(), "bucket".into(), "test/".into(), mode).unwrap();
        let image = images.put(MessageImageFormat::Png, png).await.unwrap();
        let reopened = SessionImageObjects::s3(client, "bucket".into(), "test".into(), mode).unwrap();
        assert_eq!(reopened.get(image).await.unwrap(), png);
        (reopened, image)
    }.with_subscriber(subscriber).await;
    let key = state.lock().unwrap().keys()
        .find(|key| key.starts_with("test.waltr-message-objects/image/")).unwrap().clone();
    let owner_key = state
        .lock()
        .unwrap()
        .keys()
        .find_map(|key| key.strip_prefix("test.waltr-message-objects/image-capacity-v1/"))
        .unwrap()
        .to_owned();
    let captured = String::from_utf8(trace.0.lock().unwrap().clone()).unwrap();
    assert!(captured.contains("trace capture enabled"));
    assert!(!captured.contains(key.rsplit('/').next().unwrap()));
    assert!(!captured.contains(&owner_key));
    assert!(!captured.contains("test-issuer:test-subject"));
    assert!(!captured.contains("saved-image-payload"));
    state.lock().unwrap().insert(key, b"\x89PNG\r\n\x1a\nbad-image---payload".to_vec());
    assert_eq!(reopened.get(image).await, Err(crate::session::image_objects::ImageObjectError::Corrupt));
    assert_eq!(reopened.put(MessageImageFormat::Png, png).await, Err(crate::session::image_objects::ImageObjectError::Corrupt));
    stop.cancel();
    server.await.unwrap();
}
