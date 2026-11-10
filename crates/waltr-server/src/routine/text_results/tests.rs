use super::*;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct FakeStore {
    selected: Arc<Mutex<Option<Selected>>>,
}
type Selected = (TextResultDescriptor, Vec<u8>);
fn authorize(scope: &InvocationTextScope) -> Result<(), TextResultError> {
    if scope.principal() == "owner" && scope.invocation_id() == "durable" {
        Ok(())
    } else {
        Err(TextResultError::Denied)
    }
}
impl TextResultStore for FakeStore {
    fn publish(
        &self,
        scope: InvocationTextScope,
        name: String,
        helper_id: Option<String>,
        mut source: Box<dyn TextSource>,
        cancellation: CancellationToken,
    ) -> TextFuture<TextResultDescriptor> {
        let selected = self.selected.clone();
        Box::pin(async move {
            authorize(&scope)?;
            if !bounded_identifier(&name, 64)
                || helper_id
                    .as_deref()
                    .is_some_and(|id| !bounded_identifier(id, 128))
            {
                return Err(TextResultError::Invalid);
            }
            let _credit = reserve(PUBLISH_CREDIT, &cancellation).await?;
            let mut bytes = Vec::with_capacity(MAX_TEXT_BYTES);
            let mut chunk = vec![0; MAX_CHUNK_BYTES];
            loop {
                let count = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(TextResultError::Cancelled),
                    count = source.read(&mut chunk) => count?,
                };
                if count > chunk.len() {
                    return Err(TextResultError::Invalid);
                }
                if count == 0 {
                    break;
                }
                if bytes.len() + count > MAX_TEXT_BYTES {
                    return Err(TextResultError::Quota);
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| TextResultError::Invalid)?;
            let mut descriptor = TextResultDescriptor {
                version: 1,
                id: "00000000-0000-4000-8000-000000000000".into(),
                name,
                sha256: Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                utf8_bytes: bytes.len() as u64,
                encoded_charge: 0,
                helper_id,
            };
            loop {
                let charge = serde_json::to_vec(
                    &serde_json::json!({ "descriptor": descriptor, "text": text }),
                )
                .unwrap()
                .len() as u64;
                if charge == descriptor.encoded_charge {
                    break;
                }
                descriptor.encoded_charge = charge;
            }
            let mut slot = selected.lock().unwrap();
            if slot.is_some() {
                return Err(TextResultError::Conflict);
            }
            *slot = Some((descriptor.clone(), bytes));
            Ok(descriptor)
        })
    }
    fn list(&self, scope: InvocationTextScope) -> TextFuture<Vec<TextResultDescriptor>> {
        let selected = self.selected.clone();
        Box::pin(async move {
            authorize(&scope)?;
            Ok(selected
                .lock()
                .unwrap()
                .iter()
                .map(|(descriptor, _)| descriptor.clone())
                .collect())
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
        let selected = self.selected.clone();
        Box::pin(async move {
            authorize(&scope)?;
            if max == 0 || max as usize > MAX_CHUNK_BYTES {
                return Err(TextResultError::Invalid);
            }
            let credit = reserve(MAX_CHUNK_BYTES as u32, &cancellation).await?;
            let slice = {
                let slot = selected.lock().unwrap();
                let (descriptor, bytes) = slot.as_ref().ok_or(TextResultError::Denied)?;
                if id != descriptor.id {
                    return Err(TextResultError::Denied);
                }
                let start = usize::try_from(offset).map_err(|_| TextResultError::Invalid)?;
                if start > bytes.len() {
                    return Err(TextResultError::Invalid);
                }
                let end = start
                    .checked_add(max as usize)
                    .ok_or(TextResultError::Invalid)?
                    .min(bytes.len());
                bytes[start..end].to_vec()
            };
            Ok(TextChunk::new(slice, credit))
        })
    }
}
struct FixedSource(bool);
impl TextSource for FixedSource {
    fn read<'a>(&'a mut self, destination: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async move {
            if self.0 {
                return Ok(0);
            }
            self.0 = true;
            destination[..8].copy_from_slice(b"business");
            Ok(8)
        })
    }
}

#[tokio::test]
async fn text_result_object_safe_fake_substitution_without_sql_engine_or_media() {
    let store: Arc<dyn TextResultStore> = Arc::new(FakeStore::default());
    let scope = InvocationTextScope::new("owner".into(), "durable".into()).unwrap();
    let descriptor = store
        .publish(
            scope.clone(),
            "notes".into(),
            None,
            Box::new(FixedSource(false)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.list(scope.clone()).await.unwrap(),
        vec![descriptor.clone()]
    );
    let chunk = store
        .read(scope, descriptor.id, 0, 64, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(chunk.bytes(), b"business");
    assert!(!format!("{chunk:?}").contains("business"));
}
