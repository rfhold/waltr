//! Bounded staging for image uploads. A staged image is never a durable reference.

use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};
use uuid::Uuid;
use waltr_agent::{
    MAX_TRANSIENT_ATTACHMENT_BYTES, MAX_TRANSIENT_SOURCE_BYTES, MessageImageDescriptor,
    MessageImageFormat, UserMessagePart, has_user_message_content,
};

use super::image_objects::{ImageObjectError, SessionImageObjects};

const MAX_CHUNK: usize = 64 * 1024;
const MAX_IMAGES_PER_MESSAGE: usize = 4;
const MAX_STAGED_UPLOADS: usize = 64;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PRINCIPAL_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const UPLOAD_TTL: Duration = Duration::from_secs(120);
const MAX_MESSAGE_PARTS: usize = 32;
const MAX_MESSAGE_TEXT_BYTES: usize = 16_384;

/// Exact authority supplied by the session service after access validation.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageUploadScope {
    principal: String,
    route: String,
    session_id: waltr_agent::SessionId,
    incarnation: String,
}

impl ImageUploadScope {
    /// Pin authority resolved by the service, not values supplied in an RPC body.
    /// The route identity includes the Remote grant generation, when applicable.
    #[must_use]
    pub fn new(
        principal: String,
        route: String,
        session_id: waltr_agent::SessionId,
        incarnation: String,
    ) -> Self {
        Self {
            principal,
            route,
            session_id,
            incarnation,
        }
    }
}

impl std::fmt::Debug for ImageUploadScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageUploadScope").finish_non_exhaustive()
    }
}

/// An opaque, single-use staging handle. Scope validation remains mandatory.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageUploadId(Uuid);

impl ImageUploadId {
    /// Parse a wire handle.
    #[must_use]
    pub fn parse(handle: &str) -> Option<Self> {
        Uuid::parse_str(handle).ok().map(Self)
    }

    /// Encode a handle for the client that started the upload.
    #[must_use]
    pub fn token(self) -> String {
        self.0.to_string()
    }
}

impl std::fmt::Debug for ImageUploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageUploadId").finish_non_exhaustive()
    }
}

/// Trusted upload error category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageUploadError {
    /// Malformed length, offset, chunk, format, or batch.
    Invalid,
    /// The handle is absent, already consumed, or outside this scope.
    Missing,
    /// The exact scope's handle expired and is still known to this pool.
    Expired,
    /// The handle is in the wrong stage.
    Conflict,
    /// The staging pool has insufficient credit.
    Capacity,
    /// The staging pool is not usable.
    Unavailable,
}

struct Upload {
    scope: ImageUploadScope,
    expires: Instant,
    format: MessageImageFormat,
    declared_bytes: u32,
    bytes: Vec<u8>,
    verified: Option<MessageImageDescriptor>,
    credit: UploadCredit,
}

struct UploadCredit {
    _principal_bytes: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
}

/// Verified, process-local bytes with a staging reservation. The reservation
/// follows the bytes through enqueue, publication, cancellation, and drop.
pub struct VerifiedImageUpload {
    descriptor: MessageImageDescriptor,
    bytes: Vec<u8>,
    _credit: UploadCredit,
}

impl std::fmt::Debug for VerifiedImageUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedImageUpload")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl VerifiedImageUpload {
    /// Checked contents to publish in the authorized message append path.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Verified integrity metadata.
    #[must_use]
    pub const fn descriptor(&self) -> MessageImageDescriptor {
        self.descriptor
    }
}

/// A validated ordered message with single-use staging credit still held.
/// Dropping it releases the credit without restoring its handles.
pub struct ClaimedImageMessage {
    principal: String,
    parts: Vec<UserMessagePart<VerifiedImageUpload>>,
}

impl std::fmt::Debug for ClaimedImageMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimedImageMessage")
            .finish_non_exhaustive()
    }
}

impl ClaimedImageMessage {
    /// Verified descriptors in message order, for quota reservation before any write.
    pub fn descriptors(&self) -> impl Iterator<Item = MessageImageDescriptor> + '_ {
        self.parts.iter().filter_map(|part| match part {
            UserMessagePart::Text { .. } => None,
            UserMessagePart::Image { image } => Some(image.descriptor()),
        })
    }

    /// Complete verified publication before appending these parts to a session.
    /// A failed or cancelled publication may leave unreferenced immutable objects.
    ///
    /// # Errors
    /// Rejects storage failures without returning any appendable parts.
    pub async fn publish(
        self,
        objects: &SessionImageObjects,
    ) -> Result<Vec<UserMessagePart<MessageImageDescriptor>>, ImageObjectError> {
        let bytes = self
            .descriptors()
            .map(|image| u64::from(image.bytes()))
            .sum();
        let mut reservation = objects.reserve(&self.principal, bytes).await?;
        let mut ready = Vec::with_capacity(self.parts.len());
        for part in self.parts {
            ready.push(match part {
                UserMessagePart::Text { text } => UserMessagePart::Text { text },
                UserMessagePart::Image { image: upload } => {
                    let image = reservation
                        .put(upload.descriptor().format(), upload.bytes())
                        .await?;
                    UserMessagePart::Image { image }
                }
            });
        }
        Ok(ready)
    }
}

#[derive(Default)]
struct Staging {
    pending: HashMap<ImageUploadId, Upload>,
    expired: VecDeque<(ImageUploadId, ImageUploadScope, Instant)>,
    principals: HashMap<String, Weak<Semaphore>>,
}

impl Staging {
    fn reap(&mut self, now: Instant) {
        self.expired.retain(|(_, _, until)| *until > now);
        self.pending.retain(|id, upload| {
            if upload.expires > now {
                return true;
            }
            let until = upload.expires + UPLOAD_TTL;
            if until > now {
                if self.expired.len() == MAX_STAGED_UPLOADS {
                    self.expired.pop_front();
                }
                self.expired.push_back((*id, upload.scope.clone(), until));
            }
            false
        });
        self.principals
            .retain(|_, credit| credit.strong_count() > 0);
    }

    fn principal_credit(&mut self, principal: &str) -> Arc<Semaphore> {
        if let Some(credit) = self.principals.get(principal).and_then(Weak::upgrade) {
            return credit;
        }
        let credit = Arc::new(Semaphore::new(MAX_PRINCIPAL_SOURCE_BYTES));
        self.principals
            .insert(principal.to_owned(), Arc::downgrade(&credit));
        credit
    }

    fn scoped(
        &mut self,
        scope: &ImageUploadScope,
        id: ImageUploadId,
    ) -> Result<&mut Upload, ImageUploadError> {
        if let Some(entry) = self
            .pending
            .get_mut(&id)
            .filter(|entry| &entry.scope == scope)
        {
            return Ok(entry);
        }
        if self
            .expired
            .iter()
            .any(|(expired, owner, _)| *expired == id && owner == scope)
        {
            return Err(ImageUploadError::Expired);
        }
        Err(ImageUploadError::Missing)
    }
}

/// One process-local staging pool per session owner service.
pub struct ImageUploads {
    entries: Mutex<Staging>,
    credit: Arc<Semaphore>,
    slots: Arc<Semaphore>,
}

impl std::fmt::Debug for ImageUploads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageUploads").finish_non_exhaustive()
    }
}

impl Default for ImageUploads {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            credit: Arc::new(Semaphore::new(MAX_SOURCE_BYTES)),
            slots: Arc::new(Semaphore::new(MAX_STAGED_UPLOADS)),
        }
    }
}

impl ImageUploads {
    /// Reserve memory for a declared image. The lease has an absolute lifetime.
    ///
    /// # Errors
    /// Rejects invalid sizes or a full pool.
    pub fn begin(
        &self,
        scope: ImageUploadScope,
        format: MessageImageFormat,
        declared_bytes: u32,
    ) -> Result<ImageUploadId, ImageUploadError> {
        if declared_bytes == 0 || declared_bytes as usize > MAX_TRANSIENT_ATTACHMENT_BYTES {
            return Err(ImageUploadError::Invalid);
        }
        let mut entries = self.active_entries()?;
        if entries
            .pending
            .values()
            .filter(|entry| entry.scope == scope)
            .count()
            >= MAX_IMAGES_PER_MESSAGE
        {
            return Err(ImageUploadError::Capacity);
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ImageUploadError::Capacity)?;
        let aggregate = self
            .credit
            .clone()
            .try_acquire_many_owned(declared_bytes)
            .map_err(|_| ImageUploadError::Capacity)?;
        let credit = UploadCredit {
            _principal_bytes: entries
                .principal_credit(&scope.principal)
                .try_acquire_many_owned(declared_bytes)
                .map_err(|_| ImageUploadError::Capacity)?,
            _bytes: aggregate,
            _slot: slot,
        };
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(declared_bytes as usize)
            .map_err(|_| ImageUploadError::Capacity)?;
        if bytes.capacity() > declared_bytes as usize {
            return Err(ImageUploadError::Capacity);
        }
        let id = ImageUploadId(Uuid::new_v4());
        match entries.pending.entry(id) {
            Entry::Occupied(_) => return Err(ImageUploadError::Unavailable),
            Entry::Vacant(slot) => slot.insert(Upload {
                scope,
                expires: Instant::now() + UPLOAD_TTL,
                format,
                declared_bytes,
                bytes,
                verified: None,
                credit,
            }),
        };
        Ok(id)
    }

    /// Append a bounded chunk at the exact next offset.
    ///
    /// # Errors
    /// Rejects wrong scope or stage, out-of-order chunks, or excess bytes.
    pub fn write(
        &self,
        scope: &ImageUploadScope,
        id: ImageUploadId,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), ImageUploadError> {
        let mut entries = self.active_entries()?;
        let entry = entries.scoped(scope, id)?;
        if entry.verified.is_some() {
            return Err(ImageUploadError::Conflict);
        }
        if bytes.is_empty()
            || bytes.len() > MAX_CHUNK
            || entry.bytes.len() != offset as usize
            || entry.bytes.len() + bytes.len() > entry.declared_bytes as usize
        {
            return Err(ImageUploadError::Invalid);
        }
        entry.bytes.extend_from_slice(bytes);
        Ok(())
    }

    /// Verify exact bytes and seal the pending upload. This does not make the image readable.
    ///
    /// # Errors
    /// Rejects wrong scope, incomplete data, or wrong format.
    pub fn seal(
        &self,
        scope: &ImageUploadScope,
        id: ImageUploadId,
    ) -> Result<(), ImageUploadError> {
        let mut entries = self.active_entries()?;
        let entry = entries.scoped(scope, id)?;
        if entry.verified.is_some() {
            return Ok(());
        }
        if entry.bytes.len() != entry.declared_bytes as usize {
            return Err(ImageUploadError::Invalid);
        }
        let Some(verified) = MessageImageDescriptor::for_bytes(entry.format, &entry.bytes) else {
            entries.pending.remove(&id);
            return Err(ImageUploadError::Invalid);
        };
        entry.verified = Some(verified);
        Ok(())
    }

    /// Consume a complete batch in request order, or leave every upload intact.
    ///
    /// # Errors
    /// Rejects duplicate handles, unsealed uploads, expired leases, or scope mismatches.
    pub fn take(
        &self,
        scope: &ImageUploadScope,
        ids: &[ImageUploadId],
    ) -> Result<Vec<VerifiedImageUpload>, ImageUploadError> {
        if ids.len() > MAX_IMAGES_PER_MESSAGE
            || ids.iter().copied().collect::<HashSet<_>>().len() != ids.len()
        {
            return Err(ImageUploadError::Invalid);
        }
        let mut entries = self.active_entries()?;
        let mut total = 0;
        for id in ids {
            let entry = entries.scoped(scope, *id)?;
            if entry.verified.is_none() {
                return Err(ImageUploadError::Conflict);
            }
            total += entry.bytes.len();
        }
        if total > MAX_TRANSIENT_SOURCE_BYTES {
            return Err(ImageUploadError::Capacity);
        }
        Ok(ids
            .iter()
            .map(|id| {
                let entry = entries
                    .pending
                    .remove(id)
                    .expect("validated under the staging lock");
                VerifiedImageUpload {
                    descriptor: entry.verified.expect("sealed under the staging lock"),
                    bytes: entry.bytes,
                    _credit: entry.credit,
                }
            })
            .collect())
    }

    /// Validate ordered message parts and claim all uploads without writing objects.
    ///
    /// # Errors
    /// Rejects invalid content or an invalid upload batch without consuming handles.
    pub fn claim_message(
        &self,
        scope: &ImageUploadScope,
        parts: Vec<UserMessagePart<ImageUploadId>>,
    ) -> Result<ClaimedImageMessage, ImageUploadError> {
        if parts.len() > MAX_MESSAGE_PARTS {
            return Err(ImageUploadError::Invalid);
        }
        let mut text_bytes = 0usize;
        let mut handles = Vec::new();
        for part in &parts {
            match part {
                UserMessagePart::Text { text } => {
                    text_bytes = text_bytes.saturating_add(text.len());
                }
                UserMessagePart::Image { image } => handles.push(*image),
            }
        }
        if text_bytes > MAX_MESSAGE_TEXT_BYTES || !has_user_message_content(&parts) {
            return Err(ImageUploadError::Invalid);
        }
        let mut claimed = self.take(scope, &handles)?.into_iter();
        let parts = parts
            .into_iter()
            .map(|part| match part {
                UserMessagePart::Text { text } => UserMessagePart::Text { text },
                UserMessagePart::Image { .. } => UserMessagePart::Image {
                    image: claimed.next().expect("one claim per image part"),
                },
            })
            .collect();
        Ok(ClaimedImageMessage {
            principal: scope.principal.clone(),
            parts,
        })
    }

    /// Release a staged upload that still belongs to this scope.
    ///
    /// # Errors
    /// Rejects expired, absent, consumed, or out-of-scope handles.
    pub fn abort(
        &self,
        scope: &ImageUploadScope,
        id: ImageUploadId,
    ) -> Result<(), ImageUploadError> {
        let mut entries = self.active_entries()?;
        entries.scoped(scope, id)?;
        entries.pending.remove(&id);
        Ok(())
    }

    /// Release expired staging even when no new requests arrive. The service
    /// may call this on its owned maintenance timer; no task is spawned here.
    ///
    /// # Errors
    /// Returns an error if the staging pool is not usable.
    pub fn reap_expired(&self) -> Result<(), ImageUploadError> {
        drop(self.active_entries()?);
        Ok(())
    }

    fn active_entries(&self) -> Result<std::sync::MutexGuard<'_, Staging>, ImageUploadError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| ImageUploadError::Unavailable)?;
        entries.reap(Instant::now());
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nmessage";

    fn scope(principal: &str, incarnation: &str) -> ImageUploadScope {
        ImageUploadScope::new(
            principal.into(),
            "local".into(),
            waltr_agent::SessionId::new("s"),
            incarnation.into(),
        )
    }

    #[test]
    fn exact_scope_single_use_atomic_batch_and_expiry() {
        let registry = ImageUploads::default();
        let owner = scope("alice", "first");
        let id = registry
            .begin(owner.clone(), MessageImageFormat::Png, PNG.len() as u32)
            .unwrap();
        assert_eq!(
            registry.write(&scope("bob", "first"), id, 0, PNG),
            Err(ImageUploadError::Missing)
        );
        assert_eq!(
            registry.write(&scope("alice", "second"), id, 0, PNG),
            Err(ImageUploadError::Missing)
        );
        let other_session = ImageUploadScope {
            session_id: waltr_agent::SessionId::new("other"),
            ..owner.clone()
        };
        assert_eq!(
            registry.write(&other_session, id, 0, PNG),
            Err(ImageUploadError::Missing)
        );
        assert_eq!(
            registry.write(&owner, id, 1, PNG),
            Err(ImageUploadError::Invalid)
        );
        registry.write(&owner, id, 0, PNG).unwrap();
        registry.seal(&owner, id).unwrap();

        let pending = registry
            .begin(owner.clone(), MessageImageFormat::Png, PNG.len() as u32)
            .unwrap();
        assert_eq!(
            registry.take(&owner, &[id, pending]).unwrap_err(),
            ImageUploadError::Conflict
        );
        let claimed = registry.take(&owner, &[id]).unwrap();
        assert_eq!(claimed[0].bytes(), PNG);
        assert_eq!(
            registry.take(&owner, &[id]).unwrap_err(),
            ImageUploadError::Missing
        );
        assert_eq!(
            registry.credit.available_permits(),
            MAX_SOURCE_BYTES - 2 * PNG.len()
        );
        drop(claimed);

        registry
            .entries
            .lock()
            .unwrap()
            .pending
            .get_mut(&pending)
            .unwrap()
            .expires = Instant::now();
        registry.reap_expired().unwrap();
        assert_eq!(
            registry.write(&scope("bob", "first"), pending, 0, PNG),
            Err(ImageUploadError::Missing)
        );
        assert_eq!(
            registry.write(&owner, pending, 0, PNG),
            Err(ImageUploadError::Expired)
        );
        assert_eq!(registry.credit.available_permits(), MAX_SOURCE_BYTES);
        assert_eq!(registry.slots.available_permits(), MAX_STAGED_UPLOADS);
    }

    #[tokio::test]
    async fn prepare_publishes_images_in_order_only_for_the_pinned_route() {
        let registry = ImageUploads::default();
        let objects = SessionImageObjects::memory(1024);
        let owner = scope("alice", "first");
        let old_grant = ImageUploadScope {
            route: "old-grant".into(),
            ..owner.clone()
        };
        let id = registry
            .begin(owner.clone(), MessageImageFormat::Png, PNG.len() as u32)
            .unwrap();
        registry.write(&owner, id, 0, PNG).unwrap();
        registry.seal(&owner, id).unwrap();
        let parts = vec![
            UserMessagePart::Text {
                text: "before".into(),
            },
            UserMessagePart::Image { image: id },
            UserMessagePart::Text {
                text: "after".into(),
            },
        ];
        assert_eq!(
            registry
                .claim_message(&old_grant, parts.clone())
                .unwrap_err(),
            ImageUploadError::Missing
        );
        let claimed = registry.claim_message(&owner, parts).unwrap();
        let descriptor = claimed.descriptors().next().unwrap();
        assert_eq!(
            objects.get(descriptor).await,
            Err(ImageObjectError::Missing)
        );
        let prepared = claimed.publish(&objects).await.unwrap();
        let [
            UserMessagePart::Text { text: before },
            UserMessagePart::Image { image },
            UserMessagePart::Text { text: after },
        ] = prepared.as_slice()
        else {
            panic!("ordered parts changed")
        };
        assert_eq!((before.as_str(), after.as_str()), ("before", "after"));
        assert_eq!(objects.get(*image).await.unwrap(), PNG);
    }

    #[test]
    fn oversized_batch_does_not_consume_any_uploads() {
        let registry = ImageUploads::default();
        let owner = scope("alice", "first");
        let mut image = vec![0; MAX_TRANSIENT_ATTACHMENT_BYTES];
        image[..8].copy_from_slice(&PNG[..8]);
        let mut ids = Vec::new();
        for _ in 0..3 {
            let id = registry
                .begin(owner.clone(), MessageImageFormat::Png, image.len() as u32)
                .unwrap();
            for (index, chunk) in image.chunks(MAX_CHUNK).enumerate() {
                registry
                    .write(&owner, id, (index * MAX_CHUNK) as u32, chunk)
                    .unwrap();
            }
            registry.seal(&owner, id).unwrap();
            ids.push(id);
        }
        assert_eq!(
            registry.take(&owner, &ids).unwrap_err(),
            ImageUploadError::Capacity
        );
        assert_eq!(registry.take(&owner, &ids[..2]).unwrap().len(), 2);
        registry.abort(&owner, ids[2]).unwrap();
        assert_eq!(registry.credit.available_permits(), MAX_SOURCE_BYTES);
    }

    fn stage(registry: &ImageUploads, owner: &ImageUploadScope, bytes: &[u8]) -> ImageUploadId {
        let id = registry
            .begin(owner.clone(), MessageImageFormat::Png, bytes.len() as u32)
            .unwrap();
        for (index, chunk) in bytes.chunks(MAX_CHUNK).enumerate() {
            registry
                .write(owner, id, (index * MAX_CHUNK) as u32, chunk)
                .unwrap();
        }
        registry.seal(owner, id).unwrap();
        id
    }

    #[test]
    fn claimed_images_still_hold_slots_and_drop_does_not_restore_handles() {
        let registry = ImageUploads::default();
        let mut claims = Vec::new();
        for index in 0..MAX_STAGED_UPLOADS {
            let owner = scope("alice", &index.to_string());
            let id = stage(&registry, &owner, PNG);
            claims.push(
                registry
                    .claim_message(&owner, vec![UserMessagePart::Image { image: id }])
                    .unwrap(),
            );
        }
        let owner = scope("alice", "first");
        assert_eq!(
            registry.begin(owner.clone(), MessageImageFormat::Png, PNG.len() as u32),
            Err(ImageUploadError::Capacity)
        );
        drop(claims);
        let id = stage(&registry, &owner, PNG);
        let claimed = registry
            .claim_message(&owner, vec![UserMessagePart::Image { image: id }])
            .unwrap();
        drop(claimed);
        assert_eq!(registry.abort(&owner, id), Err(ImageUploadError::Missing));
        assert_eq!(registry.credit.available_permits(), MAX_SOURCE_BYTES);
        assert_eq!(registry.slots.available_permits(), MAX_STAGED_UPLOADS);
    }

    #[test]
    fn abort_releases_credit_and_bad_signature_cannot_be_resealed() {
        let registry = ImageUploads::default();
        let owner = scope("alice", "first");
        let id = stage(&registry, &owner, PNG);
        assert_eq!(
            registry.abort(&scope("bob", "first"), id),
            Err(ImageUploadError::Missing)
        );
        registry.abort(&owner, id).unwrap();
        assert_eq!(registry.seal(&owner, id), Err(ImageUploadError::Missing));

        let id = registry
            .begin(owner.clone(), MessageImageFormat::Jpeg, PNG.len() as u32)
            .unwrap();
        assert_eq!(registry.seal(&owner, id), Err(ImageUploadError::Invalid));
        registry.write(&owner, id, 0, PNG).unwrap();
        assert_eq!(registry.seal(&owner, id), Err(ImageUploadError::Invalid));
        assert_eq!(registry.seal(&owner, id), Err(ImageUploadError::Missing));
        assert_eq!(registry.credit.available_permits(), MAX_SOURCE_BYTES);
        assert_eq!(registry.slots.available_permits(), MAX_STAGED_UPLOADS);
    }

    #[tokio::test]
    async fn image_batch_failure_returns_no_message_and_does_not_restore_handles() {
        let registry = ImageUploads::default();
        let objects = SessionImageObjects::memory(PNG.len());
        let owner = scope("alice", "first");
        let first = stage(&registry, &owner, PNG);
        let second = stage(&registry, &owner, b"\x89PNG\r\n\x1a\nsecond");
        let parts = vec![
            UserMessagePart::Image { image: first },
            UserMessagePart::Image { image: second },
        ];
        let claimed = registry.claim_message(&owner, parts.clone()).unwrap();
        let descriptors: Vec<_> = claimed.descriptors().collect();
        assert_eq!(
            claimed.publish(&objects).await,
            Err(ImageObjectError::Capacity)
        );
        assert_eq!(objects.get(descriptors[0]).await.unwrap(), PNG);
        assert_eq!(
            objects.get(descriptors[1]).await,
            Err(ImageObjectError::Missing)
        );
        assert_eq!(
            registry.claim_message(&owner, parts).unwrap_err(),
            ImageUploadError::Missing
        );
        assert_eq!(registry.credit.available_permits(), MAX_SOURCE_BYTES);
        assert_eq!(registry.slots.available_permits(), MAX_STAGED_UPLOADS);
    }

    #[tokio::test]
    async fn cancelling_image_publication_releases_in_flight_credit() {
        use crate::routine_objects::{ImmutableObjects, ObjectFuture, ObjectRef};

        struct PendingStore;
        impl ImmutableObjects for PendingStore {
            fn put<'a>(&'a self, _: &'a ObjectRef, _: &'a [u8]) -> ObjectFuture<'a, ()> {
                Box::pin(std::future::pending())
            }
            fn get<'a>(&'a self, _: &'a ObjectRef) -> ObjectFuture<'a, Vec<u8>> {
                unreachable!("put did not finish")
            }
        }

        let registry = ImageUploads::default();
        let objects = SessionImageObjects::test_durable(PendingStore);
        let owner = scope("alice", "first");
        let id = stage(&registry, &owner, PNG);
        let parts = vec![UserMessagePart::Image { image: id }];
        let claimed = registry.claim_message(&owner, parts.clone()).unwrap();
        {
            use std::{future::Future as _, task::Poll};
            let mut publication = std::pin::pin!(claimed.publish(&objects));
            std::future::poll_fn(|cx| {
                assert!(publication.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert_eq!(
                registry.credit.available_permits(),
                MAX_SOURCE_BYTES - PNG.len()
            );
        }
        assert_eq!(
            registry.claim_message(&owner, parts).unwrap_err(),
            ImageUploadError::Missing
        );
        assert_eq!(registry.credit.available_permits(), MAX_SOURCE_BYTES);
        assert_eq!(registry.slots.available_permits(), MAX_STAGED_UPLOADS);
    }
    #[test]
    fn pending_byte_limit_is_shared_across_a_principals_sessions() {
        let registry = ImageUploads::default();
        let bytes = MAX_TRANSIENT_ATTACHMENT_BYTES as u32;
        for n in 0..4 {
            registry
                .begin(
                    scope("alice", &n.to_string()),
                    MessageImageFormat::Png,
                    bytes,
                )
                .unwrap();
        }
        assert_eq!(
            registry.begin(scope("alice", "next"), MessageImageFormat::Png, 1),
            Err(ImageUploadError::Capacity)
        );
        registry
            .begin(scope("bob", "next"), MessageImageFormat::Png, bytes)
            .unwrap();
    }

    #[test]
    fn claimed_batches_hold_principal_credit_until_dropped() {
        let registry = ImageUploads::default();
        let mut image = vec![0; MAX_TRANSIENT_ATTACHMENT_BYTES];
        image[..8].copy_from_slice(&PNG[..8]);
        let claims: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|session| {
                let owner = ImageUploadScope {
                    session_id: waltr_agent::SessionId::new(session),
                    ..scope("alice", "first")
                };
                let parts = (0..2)
                    .map(|_| UserMessagePart::Image {
                        image: stage(&registry, &owner, &image),
                    })
                    .collect();
                registry.claim_message(&owner, parts).unwrap()
            })
            .collect();
        let alice = scope("alice", "next");
        let bob = scope("bob", "next");
        assert_eq!(
            registry.begin(alice.clone(), MessageImageFormat::Png, PNG.len() as u32),
            Err(ImageUploadError::Capacity)
        );
        let bob_upload = stage(&registry, &bob, PNG);
        drop(claims);
        let alice_upload = stage(&registry, &alice, &image);
        registry.abort(&alice, alice_upload).unwrap();
        registry.abort(&bob, bob_upload).unwrap();
        registry.reap_expired().unwrap();
        assert!(registry.entries.lock().unwrap().principals.is_empty());
        assert_eq!(registry.credit.available_permits(), MAX_SOURCE_BYTES);
        assert_eq!(registry.slots.available_permits(), MAX_STAGED_UPLOADS);
    }
}
