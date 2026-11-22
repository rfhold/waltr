//! Immutable bytes for saved message images. Access checks belong to the session
//! service; neither an image descriptor nor its digest authorizes a read.

use std::{
    collections::{HashMap, hash_map::Entry},
    fmt::Write as _,
    path::Path,
    sync::{Arc, Mutex},
};

use tracing::instrument::WithSubscriber;
use waltr_agent::{
    MAX_TRANSIENT_ATTACHMENT_BYTES, MessageImageDescriptor, MessageImageFormat,
    MessageImageLoadError, MessageImageLoadFuture, MessageImageLocation, MessageImageReference,
    MessageImageResolver, S3SessionWriteMode, TransientAttachmentBuffer,
};

use crate::routine_objects::{
    FilesystemObjects, ImmutableObjects, Namespace, ObjectRef, S3Objects,
};

mod capacity;

use capacity::{Capacity, SAVED_IMAGE_BYTES_PER_PRINCIPAL};

pub(crate) fn quota_identity(principal: &crate::access_auth::AuthenticatedPrincipal) -> String {
    // Length framing keeps the exact issuer/subject pair unambiguous.
    format!(
        "{}:{}{}",
        principal.issuer().len(),
        principal.issuer(),
        principal.subject()
    )
}

pub(crate) struct SavedImageResolver {
    objects: SessionImageObjects,
    slots: tokio::sync::Semaphore,
}

impl SavedImageResolver {
    pub(crate) fn new(objects: SessionImageObjects) -> Self {
        Self {
            objects,
            slots: tokio::sync::Semaphore::new(8),
        }
    }
}

impl MessageImageResolver for SavedImageResolver {
    fn load<'a>(
        &'a self,
        _location: MessageImageLocation<'a>,
        image: MessageImageReference,
        target: &'a mut TransientAttachmentBuffer,
    ) -> MessageImageLoadFuture<'a> {
        Box::pin(async move {
            let MessageImageReference::Saved(image) = image else {
                return Err(MessageImageLoadError::Missing);
            };
            let _permit = self
                .slots
                .try_acquire()
                .map_err(|_| MessageImageLoadError::Unavailable)?;
            let bytes = self.objects.get(image).await.map_err(|error| match error {
                ImageObjectError::Missing => MessageImageLoadError::Missing,
                ImageObjectError::Invalid | ImageObjectError::Corrupt => {
                    MessageImageLoadError::Corrupt
                }
                ImageObjectError::Capacity | ImageObjectError::Unavailable => {
                    MessageImageLoadError::Unavailable
                }
            })?;
            target
                .append(&bytes)
                .map_err(|_| MessageImageLoadError::Corrupt)
        })
    }
}

/// Internal image-store failure, with no object key or payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageObjectError {
    /// Size or format signature did not match the allowed input.
    Invalid,
    /// No object exists for the descriptor.
    Missing,
    /// Stored bytes failed verification.
    Corrupt,
    /// The image store has no capacity for this write.
    Capacity,
    /// The storage operation could not be verified.
    Unavailable,
}

impl From<crate::routine_objects::ObjectError> for ImageObjectError {
    fn from(error: crate::routine_objects::ObjectError) -> Self {
        use crate::routine_objects::ObjectError;
        match error {
            ObjectError::Invalid => Self::Invalid,
            ObjectError::Missing => Self::Missing,
            ObjectError::Corrupt => Self::Corrupt,
            ObjectError::Unavailable => Self::Unavailable,
        }
    }
}

#[derive(Default)]
struct MemoryImages {
    used: usize,
    bytes: HashMap<[u8; 32], Vec<u8>>,
}

enum Backend {
    Memory {
        state: Mutex<MemoryImages>,
        limit: usize,
    },
    Durable {
        objects: Box<dyn ImmutableObjects>,
        capacity: Capacity,
        limit: u64,
    },
}

/// Image bytes from the backend that accompanies the session record store.
#[derive(Clone)]
pub struct SessionImageObjects(Arc<Backend>);

/// Authority to attempt at most the charged batch size on one object store.
/// Dropping it never refunds a durable charge.
pub(crate) struct ReservedImageWrite<'a> {
    store: &'a SessionImageObjects,
    remaining: u64,
}

impl ReservedImageWrite<'_> {
    pub(crate) async fn put(
        &mut self,
        format: MessageImageFormat,
        bytes: &[u8],
    ) -> Result<MessageImageDescriptor, ImageObjectError> {
        let remaining = self
            .remaining
            .checked_sub(bytes.len() as u64)
            .ok_or(ImageObjectError::Invalid)?;
        // An error or cancellation must not allow the same reservation to be reused.
        self.remaining = 0;
        let descriptor = self.store.put_verified(format, bytes).await?;
        self.remaining = remaining;
        Ok(descriptor)
    }
}

impl std::fmt::Debug for SessionImageObjects {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionImageObjects")
            .finish_non_exhaustive()
    }
}

impl SessionImageObjects {
    #[cfg(test)]
    pub(super) fn test_durable(objects: impl ImmutableObjects + 'static) -> Self {
        Self::durable(
            objects,
            Capacity::Unmetered,
            SAVED_IMAGE_BYTES_PER_PRINCIPAL,
        )
    }

    fn durable(objects: impl ImmutableObjects + 'static, capacity: Capacity, limit: u64) -> Self {
        Self(Arc::new(Backend::Durable {
            objects: Box::new(objects),
            capacity,
            limit,
        }))
    }

    /// Use process-local image storage, with an explicit aggregate byte budget.
    #[must_use]
    pub fn memory(limit: usize) -> Self {
        Self(Arc::new(Backend::Memory {
            state: Mutex::default(),
            limit,
        }))
    }

    /// Put the image namespace beside the session records, so the record store
    /// never interprets image files as sessions or removes them on a record wipe.
    ///
    /// # Errors
    /// Returns an error if the session root cannot be made absolute or has no sibling.
    pub fn filesystem(session_root: &Path) -> Result<Self, ImageObjectError> {
        Self::filesystem_with_limit(session_root, SAVED_IMAGE_BYTES_PER_PRINCIPAL)
    }

    fn filesystem_with_limit(session_root: &Path, limit: u64) -> Result<Self, ImageObjectError> {
        let session_root =
            std::path::absolute(session_root).map_err(|_| ImageObjectError::Invalid)?;
        let mut sibling = session_root
            .file_name()
            .ok_or(ImageObjectError::Invalid)?
            .to_owned();
        sibling.push(".waltr-message-objects");
        let root = session_root.with_file_name(sibling);
        let objects = FilesystemObjects::new(root.clone())?;
        Ok(Self::durable(
            objects,
            Capacity::Filesystem(root.join(capacity::NAMESPACE)),
            limit,
        ))
    }

    /// Use the session store's bucket and credentials, with a separate key prefix.
    ///
    /// # Errors
    /// Returns an error for an invalid bucket or session record prefix.
    pub fn s3(
        client: aws_sdk_s3::Client,
        bucket: String,
        session_prefix: String,
        write_mode: S3SessionWriteMode,
    ) -> Result<Self, ImageObjectError> {
        Self::s3_with_limit(
            client,
            bucket,
            session_prefix,
            write_mode,
            SAVED_IMAGE_BYTES_PER_PRINCIPAL,
        )
    }

    fn s3_with_limit(
        client: aws_sdk_s3::Client,
        bucket: String,
        session_prefix: String,
        write_mode: S3SessionWriteMode,
        limit: u64,
    ) -> Result<Self, ImageObjectError> {
        // A retry after an unknown PUT could hide its outcome behind a 412.
        let client = aws_sdk_s3::Client::from_conf(
            client
                .config()
                .to_builder()
                .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
                .build(),
        );
        let prefix = session_prefix.trim_end_matches('/');
        let objects = S3Objects::new(client.clone(), bucket.clone(), prefix.into())?;
        Ok(Self::durable(
            objects,
            Capacity::S3 {
                client,
                bucket,
                prefix: format!("{prefix}.waltr-message-objects/{}", capacity::NAMESPACE),
                write_mode,
            },
            limit,
        ))
    }

    /// Charge a complete admitted batch before attempting any immutable writes.
    /// Memory continues to enforce its own aggregate bound on actual bytes.
    pub(crate) async fn reserve(
        &self,
        exact_identity: &str,
        total_bytes: u64,
    ) -> Result<ReservedImageWrite<'_>, ImageObjectError> {
        if total_bytes == 0 {
            return Err(ImageObjectError::Invalid);
        }
        if let Backend::Durable {
            capacity, limit, ..
        } = self.0.as_ref()
        {
            async { capacity.charge(exact_identity, total_bytes, *limit).await }
                .with_subscriber(tracing::Dispatch::none())
                .await?;
        }
        Ok(ReservedImageWrite {
            store: self,
            remaining: total_bytes,
        })
    }

    #[cfg(test)]
    pub(crate) async fn put(
        &self,
        format: MessageImageFormat,
        bytes: &[u8],
    ) -> Result<MessageImageDescriptor, ImageObjectError> {
        self.reserve("test-issuer:test-subject", bytes.len() as u64)
            .await?
            .put(format, bytes)
            .await
    }

    /// Verify and publish an immutable image before a session append refers to it.
    ///
    /// # Errors
    /// Rejects an invalid image, exhausted capacity, or a storage failure.
    async fn put_verified(
        &self,
        format: MessageImageFormat,
        bytes: &[u8],
    ) -> Result<MessageImageDescriptor, ImageObjectError> {
        let descriptor =
            MessageImageDescriptor::for_bytes(format, bytes).ok_or(ImageObjectError::Invalid)?;
        match self.0.as_ref() {
            Backend::Memory { state, limit } => {
                let mut state = state.lock().map_err(|_| ImageObjectError::Unavailable)?;
                let MemoryImages {
                    used,
                    bytes: objects,
                } = &mut *state;
                match objects.entry(descriptor.sha256()) {
                    Entry::Occupied(existing) if existing.get().as_slice() == bytes => {}
                    Entry::Occupied(_) => return Err(ImageObjectError::Corrupt),
                    Entry::Vacant(entry) => {
                        let next = used
                            .checked_add(bytes.len())
                            .filter(|next| next <= limit)
                            .ok_or(ImageObjectError::Capacity)?;
                        entry.insert(bytes.to_vec());
                        *used = next;
                    }
                }
            }
            Backend::Durable { objects, .. } => {
                // The S3 SDK can emit the digest-bearing request URI at TRACE.
                let reference = object_ref(descriptor)?;
                async { objects.put(&reference, bytes).await }
                    .with_subscriber(tracing::Dispatch::none())
                    .await?;
            }
        }
        Ok(descriptor)
    }

    /// Load and verify the bytes. Callers supply a descriptor from an authorized
    /// session reference, never a caller-provided digest.
    ///
    /// # Errors
    /// Rejects missing, corrupt, out-of-bounds or unavailable data.
    pub async fn get(
        &self,
        descriptor: MessageImageDescriptor,
    ) -> Result<Vec<u8>, ImageObjectError> {
        let expected = descriptor.bytes() as usize;
        if expected == 0 || expected > MAX_TRANSIENT_ATTACHMENT_BYTES {
            return Err(ImageObjectError::Invalid);
        }
        let bytes = match self.0.as_ref() {
            Backend::Memory { state, .. } => {
                let state = state.lock().map_err(|_| ImageObjectError::Unavailable)?;
                let stored = state
                    .bytes
                    .get(&descriptor.sha256())
                    .ok_or(ImageObjectError::Missing)?;
                if stored.len() != expected {
                    return Err(ImageObjectError::Corrupt);
                }
                stored.clone()
            }
            Backend::Durable { objects, .. } => {
                let reference = object_ref(descriptor)?;
                async { objects.get(&reference).await }
                    .with_subscriber(tracing::Dispatch::none())
                    .await?
            }
        };
        if !descriptor.verifies(&bytes) {
            return Err(ImageObjectError::Corrupt);
        }
        Ok(bytes)
    }
}

fn object_ref(descriptor: MessageImageDescriptor) -> Result<ObjectRef, ImageObjectError> {
    let mut digest = String::with_capacity(64);
    for byte in descriptor.sha256() {
        write!(&mut digest, "{byte:02x}").expect("String writes cannot fail");
    }
    Ok(ObjectRef::new(
        Namespace::MessageImage,
        digest,
        u64::from(descriptor.bytes()),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nmessage";

    #[tokio::test]
    async fn memory_deduplicates_verified_bytes_and_enforces_its_budget() {
        let store = SessionImageObjects::memory(PNG.len());
        let descriptor = store.put(MessageImageFormat::Png, PNG).await.unwrap();
        assert_eq!(
            store.put(MessageImageFormat::Png, PNG).await.unwrap(),
            descriptor
        );
        assert_eq!(store.get(descriptor).await.unwrap(), PNG);
        assert_eq!(
            store.put(MessageImageFormat::Jpeg, PNG).await,
            Err(ImageObjectError::Invalid)
        );
        assert_eq!(
            store
                .put(MessageImageFormat::Png, b"\x89PNG\r\n\x1a\ndifferent")
                .await,
            Err(ImageObjectError::Capacity)
        );
    }

    #[tokio::test]
    async fn filesystem_verifies_reopened_objects_without_republishing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let descriptor = SessionImageObjects::filesystem(&root)
            .unwrap()
            .put(MessageImageFormat::Png, PNG)
            .await
            .unwrap();
        let reopened = SessionImageObjects::filesystem(&root).unwrap();
        assert_eq!(reopened.get(descriptor).await.unwrap(), PNG);
        let path = tmp
            .path()
            .join("sessions.waltr-message-objects")
            .join("image")
            .join(object_ref(descriptor).unwrap().sha256);
        std::fs::write(path, b"\x89PNG\r\n\x1a\nchanged").unwrap();
        assert_eq!(
            reopened.get(descriptor).await,
            Err(ImageObjectError::Corrupt)
        );
        assert_eq!(
            reopened.put(MessageImageFormat::Png, PNG).await,
            Err(ImageObjectError::Corrupt)
        );
    }
}
