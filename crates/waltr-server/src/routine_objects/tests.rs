use super::*;
use axum::{body::{Body, Bytes}, extract::State, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::get, Router};
use std::sync::Mutex;

#[tokio::test]
async fn filesystem_roundtrip_immutable_digest_missing_and_corruption() {
    let root = crate::test_support::temp_root().join("payloads");
    let store = FilesystemObjects::new(root.clone()).unwrap();
    let object = ObjectRef::for_bytes(Namespace::Program, b"exact source").unwrap();
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

type FakeState = Arc<Mutex<Option<Vec<u8>>>>;
async fn fake_get(State(state): State<FakeState>) -> Response {
    match state.lock().unwrap().clone() {
        Some(bytes) => Response::new(Body::from(bytes)),
        None => (StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code></Error>").into_response(),
    }
}
async fn fake_put(State(state): State<FakeState>, headers: HeaderMap, bytes: Bytes) -> Response {
    assert_eq!(headers.get("if-none-match").unwrap(), "*");
    let mut stored = state.lock().unwrap();
    if stored.is_some() {
        return (StatusCode::PRECONDITION_FAILED, "<Error><Code>PreconditionFailed</Code></Error>").into_response();
    }
    *stored = Some(bytes.to_vec());
    StatusCode::OK.into_response()
}

#[tokio::test]
async fn fake_s3_roundtrip_conditional_immutability_missing_and_corruption() {
    let state: FakeState = Arc::new(Mutex::new(None));
    let app = Router::new().route("/{bucket}/{*key}", get(fake_get).put(fake_put)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let stop = tokio_util::sync::CancellationToken::new();
    let stopped = stop.clone();
    let server = tokio::spawn(async move { axum::serve(listener, app).with_graceful_shutdown(stopped.cancelled_owned()).await.unwrap(); });
    let client = crate::session::storage::s3_client(endpoint, "test".into(), "test".into(), "test".into(), true);
    let store = S3Objects::new(client, "bucket".into(), "test".into()).unwrap();
    let object = ObjectRef::for_bytes(Namespace::Text, "UTF-8 é".as_bytes()).unwrap();
    assert_eq!(store.get(&object).await, Err(ObjectError::Missing));
    store.put(&object, "UTF-8 é".as_bytes()).await.unwrap();
    store.put(&object, "UTF-8 é".as_bytes()).await.unwrap();
    assert_eq!(store.get(&object).await.unwrap(), "UTF-8 é".as_bytes());
    *state.lock().unwrap() = Some(b"corrupt".to_vec());
    assert_eq!(store.get(&object).await, Err(ObjectError::Corrupt));
    assert_eq!(store.put(&object, "UTF-8 é".as_bytes()).await, Err(ObjectError::Corrupt));
    stop.cancel();
    server.await.unwrap();
}
