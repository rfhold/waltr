use super::{DataStores, Selected, decode_inline, live};
use crate::access_auth::{AuthenticatedPrincipal, AuthenticationMethod};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex as RegistryMutex},
    time::Duration,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _},
    sync::Mutex,
    task::AbortHandle,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use waltr_datastore::{Actor, Error, MAX_OBJECT_BYTES, Namespace, Scope, object::ObjectBody};

const CAPACITY: usize = 16;
const PER_ACTOR: usize = 2;
const LIFETIME: Duration = Duration::from_secs(30);

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferOpen {
    pub(crate) store: String,
    key: String,
    size: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferHandle {
    pub(crate) store: String,
    transfer: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferChunk {
    pub(crate) store: String,
    transfer: String,
    offset: u64,
    content_base64: String,
}

enum Body {
    Upload(File),
    Download(ObjectBody),
}

struct Progress {
    body: Body,
    size: u64,
    offset: u64,
}

struct Transfer {
    progress: Mutex<Option<Progress>>,
    key: String,
    activation: Uuid,
    namespace: Namespace,
    actor: Option<Actor>,
    principal: AuthenticatedPrincipal,
    expires_at: Instant,
    closed: CancellationToken,
}

impl Transfer {
    fn matches(&self, selected: &Selected) -> bool {
        self.activation == selected.activation
            && self.namespace == selected.namespace
            && self.actor == selected.actor
    }

    async fn run<T>(&self, operation: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        if Instant::now() >= self.expires_at || !live(&self.principal) {
            return Err(Error::NotFound);
        }
        tokio::select! {
            biased;
            () = self.closed.cancelled() => Err(Error::NotFound),
            () = sleep_until(self.expires_at) => Err(Error::Unavailable),
            result = async {
                if !self.principal.revalidate_datastore_access().await {
                    return Err(Error::NotFound);
                }
                operation.await
            } => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr_datastore::{Access, Definition, Kind, object::Objects};

    fn selected(principal: &AuthenticatedPrincipal, activation: Uuid) -> Selected {
        let actor = principal.datastore_actor();
        let definition = Definition {
            kind: Kind::Object,
            access: Access {
                scope: Scope::Shared,
            },
            collections: BTreeMap::new(),
            relations: BTreeMap::new(),
        };
        Selected {
            namespace: Namespace::resolve("files", &definition, actor.as_ref()).unwrap(),
            definition,
            activation,
            actor,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_upload_and_download_release_resources_without_another_request() {
        let transfers = Transfers::default();
        let owner = AuthenticatedPrincipal::apps_test_principal("owner");
        let reader = AuthenticatedPrincipal::anonymous_disabled();
        let activation = Uuid::new_v4();
        let owner_store = selected(&owner, activation);
        let reader_store = selected(&reader, activation);

        let root = tempfile::tempdir().unwrap();
        let objects = Objects::filesystem(root.path()).unwrap();
        objects
            .create(
                &owner_store.namespace,
                "download",
                owner_store.actor.as_ref().unwrap(),
                &b"file"[..],
            )
            .await
            .unwrap();
        let opened = objects
            .read(&reader_store.namespace, "download")
            .await
            .unwrap();
        let upload = File::from_std(tempfile::tempfile().unwrap());

        let mut watched = Vec::new();
        for (principal, store, key, body, size) in [
            (&owner, &owner_store, "upload", Body::Upload(upload), 0),
            (
                &reader,
                &reader_store,
                "download",
                Body::Download(opened.body),
                opened.info.size,
            ),
        ] {
            let (id, entry, guard) = transfers.reserve(store, principal, key.into()).unwrap();
            *entry.progress.try_lock().unwrap() = Some(Progress {
                body,
                size,
                offset: 0,
            });
            guard.keep();
            watched.push((id, Arc::downgrade(&entry), entry.closed.clone()));
        }

        let upload_id = watched[0].0;
        assert!(transfers.lookup(upload_id, &owner_store).is_ok());
        assert!(matches!(
            transfers.lookup(upload_id, &reader_store),
            Err(Error::NotFound)
        ));

        tokio::time::advance(LIFETIME - Duration::from_secs(1)).await;
        assert_eq!(transfers.0.lock().unwrap().len(), 2);
        tokio::time::advance(Duration::from_secs(1)).await;
        for (_, entry, closed) in watched {
            tokio::time::timeout(Duration::from_secs(1), closed.cancelled())
                .await
                .unwrap();
            assert!(entry.upgrade().is_none());
        }
        assert!(transfers.0.lock().unwrap().is_empty());
    }
}

struct Registered {
    transfer: Arc<Transfer>,
    reaper: AbortHandle,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.transfer.closed.cancel();
        self.reaper.abort();
    }
}

type Registry = RegistryMutex<BTreeMap<Uuid, Registered>>;

struct RemoveOnDrop<'a> {
    registry: &'a Registry,
    id: Option<Uuid>,
}

impl RemoveOnDrop<'_> {
    fn keep(mut self) {
        self.id = None;
    }
}

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id
            && let Ok(mut registry) = self.registry.lock()
        {
            registry.remove(&id);
        }
    }
}

#[derive(Default)]
pub(super) struct Transfers(Arc<Registry>);

impl Transfers {
    fn remove_on_drop(&self, id: Uuid) -> RemoveOnDrop<'_> {
        RemoveOnDrop {
            registry: &self.0,
            id: Some(id),
        }
    }

    fn lookup(&self, id: Uuid, selected: &Selected) -> Result<Arc<Transfer>, Error> {
        let registry = self.0.lock().map_err(|_| Error::Unavailable)?;
        registry
            .get(&id)
            .map(|entry| &entry.transfer)
            .filter(|entry| entry.matches(selected) && Instant::now() < entry.expires_at)
            .cloned()
            .ok_or(Error::NotFound)
    }

    fn reserve(
        &self,
        selected: &Selected,
        principal: &AuthenticatedPrincipal,
        key: String,
    ) -> Result<(Uuid, Arc<Transfer>, RemoveOnDrop<'_>), Error> {
        let mut registry = self.0.lock().map_err(|_| Error::Unavailable)?;
        registry.retain(|_, entry| {
            Instant::now() < entry.transfer.expires_at && live(&entry.transfer.principal)
        });
        if registry.len() >= CAPACITY
            || registry
                .values()
                .filter(|entry| entry.transfer.actor == selected.actor)
                .count()
                >= PER_ACTOR
        {
            return Err(Error::Limit);
        }
        let id = Uuid::new_v4();
        let expires_at = Instant::now() + LIFETIME;
        let transfer = Arc::new(Transfer {
            progress: Mutex::new(None),
            key,
            activation: selected.activation,
            namespace: selected.namespace.clone(),
            actor: selected.actor.clone(),
            principal: principal.clone(),
            expires_at,
            closed: CancellationToken::new(),
        });
        let access_ended = principal.access_ended();
        // Each reserved slot owns one reaper; the task does not keep the registry alive.
        let weak = Arc::downgrade(&self.0);
        let reaper = tokio::spawn(async move {
            tokio::select! {
                () = sleep_until(expires_at) => {},
                () = access_ended => {},
            }
            if let Some(registry) = weak.upgrade()
                && let Ok(mut registry) = registry.lock()
            {
                registry.remove(&id);
            }
        });
        registry.insert(
            id,
            Registered {
                transfer: transfer.clone(),
                reaper: reaper.abort_handle(),
            },
        );
        Ok((id, transfer, self.remove_on_drop(id)))
    }

    pub(super) async fn take_upload(
        &self,
        id: Uuid,
        selected: &Selected,
        actor: &Actor,
        key: &str,
    ) -> Result<File, Error> {
        let entry = self.lookup(id, selected)?;
        if entry.actor.as_ref() != Some(actor) {
            return Err(Error::NotFound);
        }
        entry
            .run(async {
                let mut slot = entry.progress.lock().await;
                let state = slot.as_ref().ok_or(Error::NotFound)?;
                if entry.key != key
                    || state.offset != state.size
                    || !matches!(state.body, Body::Upload(_))
                {
                    return Err(Error::Invalid);
                }
                let _remove = self.remove_on_drop(id);
                let Some(Progress {
                    body: Body::Upload(mut file),
                    ..
                }) = slot.take()
                else {
                    return Err(Error::NotFound);
                };
                file.rewind().await.map_err(|_| Error::Unavailable)?;
                Ok(file)
            })
            .await
    }
}

fn handle(value: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(value).map_err(|_| Error::Invalid)
}

impl DataStores {
    pub(super) async fn transfer_call(
        &self,
        principal: &AuthenticatedPrincipal,
        selected: &Selected,
        name: &str,
        action: &str,
        value: Value,
    ) -> Result<Value, Error> {
        if selected.actor.is_none()
            && (!matches!(
                action,
                "object.download" | "object.download_chunk" | "object.close_transfer"
            ) || selected.definition.access.scope != Scope::Shared
                || principal.method() != AuthenticationMethod::AnonymousDisabled)
        {
            return Err(Error::NotFound);
        }
        if !live(principal) {
            return Err(Error::NotFound);
        }
        match action {
            "object.upload" | "object.download" => {
                let request: TransferOpen = super::input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                waltr_datastore::object_key(&request.key)?;
                let requested_size = if action == "object.upload" {
                    let size = request.size.ok_or(Error::Invalid)?;
                    if size > MAX_OBJECT_BYTES as u64 {
                        return Err(Error::Limit);
                    }
                    Some(size)
                } else {
                    if request.size.is_some() {
                        return Err(Error::Invalid);
                    }
                    None
                };
                let (id, entry, remove) =
                    self.transfers.reserve(selected, principal, request.key)?;
                let size = entry
                    .run(async {
                        let (body, size) = if let Some(size) = requested_size {
                            let file = tokio::task::spawn_blocking(tempfile::tempfile)
                                .await
                                .map_err(|_| Error::Unavailable)?
                                .map_err(|_| Error::Unavailable)?;
                            (Body::Upload(File::from_std(file)), size)
                        } else {
                            let opened = self.objects.read(&entry.namespace, &entry.key).await?;
                            (Body::Download(opened.body), opened.info.size)
                        };
                        *entry.progress.lock().await = Some(Progress {
                            body,
                            size,
                            offset: 0,
                        });
                        Ok(size)
                    })
                    .await?;
                remove.keep();
                Ok(json!({"transfer":id,"size":size,"chunk_bytes":super::INLINE_BYTES}))
            }
            "object.upload_chunk" => {
                let request: TransferChunk = super::input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                let id = handle(&request.transfer)?;
                let data = decode_inline(&request.content_base64)?;
                let entry = self.transfers.lookup(id, selected)?;
                entry
                    .run(async {
                        let mut slot = entry.progress.lock().await;
                        let state = slot.as_ref().ok_or(Error::NotFound)?;
                        let next = request
                            .offset
                            .checked_add(data.len() as u64)
                            .ok_or(Error::Invalid)?;
                        if !matches!(state.body, Body::Upload(_))
                            || request.offset != state.offset
                            || data.is_empty()
                            || next > state.size
                        {
                            return Err(Error::Invalid);
                        }
                        let remove = self.transfers.remove_on_drop(id);
                        let mut state = slot.take().ok_or(Error::NotFound)?;
                        let Body::Upload(file) = &mut state.body else {
                            return Err(Error::Invalid);
                        };
                        file.write_all(&data)
                            .await
                            .map_err(|_| Error::Unavailable)?;
                        state.offset = next;
                        *slot = Some(state);
                        remove.keep();
                        Ok(json!({"next_offset":next}))
                    })
                    .await
            }
            "object.download_chunk" | "object.close_transfer" => {
                let request: TransferHandle = super::input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                let id = handle(&request.transfer)?;
                let entry = self.transfers.lookup(id, selected)?;
                entry
                    .run(async {
                        let mut slot = entry.progress.lock().await;
                        let state = slot.as_ref().ok_or(Error::NotFound)?;
                        if action == "object.close_transfer" {
                            let _remove = self.transfers.remove_on_drop(id);
                            drop(slot.take());
                            return Ok(json!({"closed":true}));
                        }
                        if !matches!(state.body, Body::Download(_)) {
                            return Err(Error::Invalid);
                        }
                        let remove = self.transfers.remove_on_drop(id);
                        let mut state = slot.take().ok_or(Error::NotFound)?;
                        let Body::Download(body) = &mut state.body else {
                            return Err(Error::Invalid);
                        };
                        let mut data =
                            vec![0; (state.size - state.offset).min(super::INLINE_BYTES as u64) as usize];
                        let offset = state.offset;
                        body.read_exact(&mut data).await.map_err(|_| Error::Conflict)?;
                        state.offset += data.len() as u64;
                        let done = state.offset == state.size;
                        if !done {
                            *slot = Some(state);
                            remove.keep();
                        }
                        use base64::{Engine as _, engine::general_purpose::STANDARD};
                        Ok(json!({"offset":offset,"eof":done,"content_base64":STANDARD.encode(data)}))
                    })
                    .await
            }
            _ => Err(Error::Invalid),
        }
    }
}
