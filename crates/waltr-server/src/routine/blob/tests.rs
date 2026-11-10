#![cfg(unix)]

use super::{
    filesystem::{FilesystemBlobStore, FilesystemConfig},
    *,
};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
struct Fixture {
    _serial: std::sync::MutexGuard<'static, ()>,
    root: PathBuf,
    store: FilesystemBlobStore,
    clock: Arc<FakeClock>,
}
pub(super) static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct DeleteGate(FilesystemBlobStore);
impl DeleteGate {
    fn new(store: &FilesystemBlobStore) -> Self {
        store.faults().delete_gate.store(true, Ordering::Release);
        Self(store.clone())
    }
}
impl Drop for DeleteGate {
    fn drop(&mut self) {
        let faults = self.0.faults();
        let _lock = faults
            .gate_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        faults.delete_gate.store(false, Ordering::Release);
        faults.gate_wake.notify_all();
    }
}
struct ReadGate(FilesystemBlobStore);
impl ReadGate {
    fn new(store: &FilesystemBlobStore) -> Self {
        store.faults().read_entered.store(false, Ordering::Release);
        store.faults().read_bytes.store(0, Ordering::Release);
        store.faults().read_gate.store(true, Ordering::Release);
        Self(store.clone())
    }
}
impl Drop for ReadGate {
    fn drop(&mut self) {
        let faults = self.0.faults();
        let _lock = faults
            .gate_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        faults.read_gate.store(false, Ordering::Release);
        faults.gate_wake.notify_all();
    }
}
async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("bounded fault observation");
}
struct RestorePermissions(PathBuf);
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
fn wait() -> Wait {
    Wait {
        cancellation: CancellationToken::new(),
        deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(30),
    }
}
fn scope(principal: &str, invocation: &str) -> BlobScope {
    BlobScope {
        principal: principal.into(),
        invocation: invocation.into(),
    }
}
fn metadata(bytes: Option<u64>) -> UploadMetadata {
    UploadMetadata {
        display_name: "notes.bin".into(),
        content_type: "application/octet-stream".into(),
        kind: BlobKind::File,
        declared_bytes: bytes,
        expected_sha256: None,
    }
}
impl Fixture {
    fn new() -> Self {
        let serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        use std::os::unix::fs::PermissionsExt;
        #[cfg(target_os = "macos")]
        let parent = {
            let temp = std::env::temp_dir().canonicalize().unwrap();
            let approved = temp.join("opencode");
            if approved.is_dir() { approved } else { temp }
        };
        #[cfg(not(target_os = "macos"))]
        let parent = PathBuf::from(std::env::var_os("HOME").expect("private test home required"));
        let root = parent.join(format!("blob-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let clock = Arc::new(FakeClock::default());
        let store = FilesystemBlobStore::new(FilesystemConfig {
            private_root: root.clone(),
            workspace_root: std::env::current_dir().unwrap(),
            clock: clock.clone(),
        })
        .unwrap();
        Self {
            root,
            store,
            clock,
            _serial: serial,
        }
    }
    async fn finish(self) {
        self.store.shutdown().await.unwrap();
        assert_eq!(std::fs::read_dir(&self.root).unwrap().count(), 0);
        std::fs::remove_dir(&self.root).unwrap();
    }
    async fn blob(
        &self,
        scope: &BlobScope,
        owner: &InvocationOwner,
        bytes: &[u8],
    ) -> CommittedBlob {
        let upload = self
            .store
            .begin(
                scope.clone(),
                owner.clone(),
                metadata(Some(bytes.len() as u64)),
                wait(),
            )
            .await
            .unwrap();
        if !bytes.is_empty() {
            self.store
                .append(scope.clone(), upload.clone(), 0, bytes.to_vec(), wait())
                .await
                .unwrap();
        }
        self.store
            .commit(scope.clone(), upload, wait())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn committed_file_digest_authentication_and_output_expiry() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let blob = f.blob(&s, &owner, b"hello").await;
    assert_eq!(
        blob.metadata().sha256,
        <[u8; 32]>::from(Sha256::digest(b"hello"))
    );
    assert!(matches!(
        f.store
            .reader(scope("bob", "one"), blob.clone(), wait())
            .await,
        Err(BlobError::Denied)
    ));
    let reader = f
        .store
        .reader(s.clone(), blob.clone(), wait())
        .await
        .unwrap();
    assert_eq!(
        f.store
            .read(ReadAccess::Invocation(s.clone()), reader.clone(), wait())
            .await
            .unwrap(),
        ReadChunk {
            bytes: b"hello".to_vec(),
            eof: true
        }
    );
    let before = f.store.usage(wait()).await.unwrap();
    let lease = f
        .store
        .transfer_output(s.clone(), blob.clone(), wait())
        .await
        .unwrap();
    assert_eq!(lease.renew(), Err(BlobError::Denied));
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, before.bytes);
    assert!(matches!(
        f.store
            .read(ReadAccess::Invocation(s.clone()), reader, wait())
            .await,
        Err(BlobError::Stale)
    ));
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, before.bytes);
    let reader = f
        .store
        .output_reader("alice".into(), lease.clone(), wait())
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .output_reader("bob".into(), lease.clone(), wait())
            .await,
        Err(BlobError::Denied)
    ));
    f.clock.0.store(OUTPUT_LEASE_MS - 1, Ordering::Release);
    assert_eq!(
        f.store
            .read(
                ReadAccess::Output {
                    principal: "alice".into()
                },
                reader.clone(),
                wait()
            )
            .await
            .unwrap()
            .bytes,
        b"hello"
    );
    assert_eq!(lease.expires_at_ms(), OUTPUT_LEASE_MS);
    f.clock.0.store(OUTPUT_LEASE_MS, Ordering::Release);
    assert!(matches!(
        f.store
            .read(
                ReadAccess::Output {
                    principal: "alice".into()
                },
                reader,
                wait()
            )
            .await,
        Err(BlobError::Stale)
    ));
    assert!(matches!(
        f.store.output_reader("alice".into(), lease, wait()).await,
        Err(BlobError::Stale)
    ));
    f.store.retry_cleanup(wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
    f.finish().await;
}

#[tokio::test]
async fn sequence_length_digest_partial_write_and_cancel_fail_closed() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    for case in 0..5 {
        let mut meta = metadata(Some(4));
        if case == 2 {
            meta.expected_sha256 = Some([0; 32]);
        }
        let upload = f
            .store
            .begin(s.clone(), owner.clone(), meta, wait())
            .await
            .unwrap();
        match case {
            0 => assert!(matches!(
                f.store
                    .append(s.clone(), upload.clone(), 1, vec![1; 4], wait())
                    .await,
                Err(BlobError::Sequence)
            )),
            1 => {
                f.store
                    .append(s.clone(), upload.clone(), 0, vec![1; 2], wait())
                    .await
                    .unwrap();
                assert!(matches!(
                    f.store.commit(s.clone(), upload.clone(), wait()).await,
                    Err(BlobError::Invalid)
                ));
            }
            2 => {
                f.store
                    .append(s.clone(), upload.clone(), 0, vec![1; 4], wait())
                    .await
                    .unwrap();
                assert!(matches!(
                    f.store.commit(s.clone(), upload.clone(), wait()).await,
                    Err(BlobError::Invalid)
                ));
            }
            3 => {
                f.store
                    .faults()
                    .partial_write
                    .store(true, Ordering::Release);
                assert!(matches!(
                    f.store
                        .append(s.clone(), upload.clone(), 0, vec![1; 4], wait())
                        .await,
                    Err(BlobError::Unavailable)
                ));
            }
            _ => {
                let w = wait();
                w.cancellation.cancel();
                assert!(matches!(
                    f.store
                        .append(s.clone(), upload.clone(), 0, vec![1; 4], w)
                        .await,
                    Err(BlobError::Cancelled)
                ));
            }
        }
        drop(upload);
        f.store.retry_cleanup(wait()).await.unwrap();
    }
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().objects, 0);
    f.finish().await;
}

#[tokio::test]
async fn failed_deletion_keeps_quota_and_retry_joins_two_workers() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let _a = f.blob(&s, &owner, b"first").await;
    let _b = f.blob(&s, &owner, b"second").await;
    f.store.faults().delete_failures.store(2, Ordering::Release);
    assert_eq!(
        f.store.close_invocation(s, owner, wait()).await,
        Err(BlobError::Cleanup)
    );
    let usage = f.store.usage(wait()).await.unwrap();
    assert_eq!(usage.bytes, 11);
    assert_eq!(usage.objects, 2);
    assert_eq!(usage.pending_deletions, 2);
    f.store.retry_cleanup(wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
    assert!(f.store.faults().deletes_peak.load(Ordering::Acquire) <= 2);
    f.finish().await;
}

#[tokio::test]
async fn images_require_matching_signature_and_cannot_be_consumed_before_commit() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let mut meta = metadata(Some(8));
    meta.kind = BlobKind::Image(ImageFormat::Png);
    meta.content_type = "image/png".into();
    let upload = f
        .store
        .begin(s.clone(), owner.clone(), meta, wait())
        .await
        .unwrap();
    f.store
        .append(s.clone(), upload.clone(), 0, b"notapng!".to_vec(), wait())
        .await
        .unwrap();
    let forged = CommittedBlob {
        id: upload.id,
        metadata: BlobMetadata {
            display_name: String::new(),
            content_type: String::new(),
            bytes: 0,
            sha256: [0; 32],
            image: None,
        },
    };
    assert!(matches!(
        f.store.reader(s.clone(), forged, wait()).await,
        Err(BlobError::Stale)
    ));
    assert!(matches!(
        f.store.commit(s.clone(), upload, wait()).await,
        Err(BlobError::Invalid)
    ));
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn declared_reservations_charge_once_and_unknown_upload_cannot_bypass_quotas() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let mut uploads = Vec::new();
    for _ in 0..8 {
        uploads.push(
            f.store
                .begin(
                    s.clone(),
                    owner.clone(),
                    metadata(Some(64 * 1024 * 1024)),
                    wait(),
                )
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        f.store.usage(wait()).await.unwrap().bytes,
        512 * 1024 * 1024
    );
    f.store
        .append(
            s.clone(),
            uploads[0].clone(),
            0,
            vec![1; CHUNK_BYTES],
            wait(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.store.usage(wait()).await.unwrap().bytes,
        512 * 1024 * 1024
    );
    let unknown = f
        .store
        .begin(s.clone(), owner.clone(), metadata(None), wait())
        .await
        .unwrap();
    assert_eq!(
        f.store.append(s.clone(), unknown, 0, vec![1], wait()).await,
        Err(BlobError::Quota)
    );
    assert!(matches!(
        f.store
            .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
            .await,
        Err(BlobError::Quota)
    ));
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
    f.finish().await;
}

#[tokio::test]
async fn image_signature_truncation_format_and_mime_mismatches_fail_closed() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    for (format, mime, signature) in [
        (
            ImageFormat::Png,
            "image/png",
            b"\x89PNG\r\n\x1a\n".as_slice(),
        ),
        (ImageFormat::Jpeg, "image/jpeg", b"\xff\xd8".as_slice()),
        (
            ImageFormat::WebP,
            "image/webp",
            b"RIFF\0\0\0\0WEBP".as_slice(),
        ),
    ] {
        let mut meta = metadata(None);
        meta.kind = BlobKind::Image(format);
        meta.content_type = "video/webm".into();
        assert!(matches!(
            f.store
                .begin(s.clone(), owner.clone(), meta.clone(), wait())
                .await,
            Err(BlobError::Invalid)
        ));
        meta.content_type = mime.into();
        for bytes in (0..signature.len())
            .map(|n| &signature[..n])
            .chain([b"not an image".as_slice()])
        {
            let upload = f
                .store
                .begin(s.clone(), owner.clone(), meta.clone(), wait())
                .await
                .unwrap();
            if !bytes.is_empty() {
                f.store
                    .append(s.clone(), upload.clone(), 0, bytes.to_vec(), wait())
                    .await
                    .unwrap();
            }
            assert!(matches!(
                f.store.commit(s.clone(), upload, wait()).await,
                Err(BlobError::Invalid)
            ));
        }
        let wrong = if format == ImageFormat::Png {
            ImageFormat::Jpeg
        } else {
            ImageFormat::Png
        };
        meta.kind = BlobKind::Image(wrong);
        meta.content_type = if wrong == ImageFormat::Png {
            "image/png"
        } else {
            "image/jpeg"
        }
        .into();
        let upload = f
            .store
            .begin(s.clone(), owner.clone(), meta, wait())
            .await
            .unwrap();
        f.store
            .append(s.clone(), upload.clone(), 0, signature.to_vec(), wait())
            .await
            .unwrap();
        assert!(matches!(
            f.store.commit(s.clone(), upload, wait()).await,
            Err(BlobError::Invalid)
        ));
    }
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
    f.finish().await;
}

#[tokio::test]
async fn image_signatures_allow_malformed_bodies_unchanged_and_enforce_encoded_cap() {
    for (format, mime, bytes) in [
        (
            ImageFormat::Png,
            "image/png",
            b"\x89PNG\r\n\x1a\nnot an image".as_slice(),
        ),
        (
            ImageFormat::Jpeg,
            "image/jpeg",
            b"\xff\xd8not an image".as_slice(),
        ),
        (
            ImageFormat::WebP,
            "image/webp",
            b"RIFF\0\0\0\0WEBPnot an image".as_slice(),
        ),
    ] {
        let f = Fixture::new();
        let s = scope("alice", "one");
        let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
        let mut meta = metadata(Some(20 * 1024 * 1024 + 1));
        meta.kind = BlobKind::Image(format);
        meta.content_type = mime.into();
        assert!(matches!(
            f.store
                .begin(s.clone(), owner.clone(), meta.clone(), wait())
                .await,
            Err(BlobError::Invalid)
        ));
        meta.declared_bytes = Some(bytes.len() as u64);
        let upload = f
            .store
            .begin(s.clone(), owner.clone(), meta, wait())
            .await
            .unwrap();
        f.store
            .append(s.clone(), upload.clone(), 0, bytes.to_vec(), wait())
            .await
            .unwrap();
        f.store.faults().read_bytes.store(0, Ordering::Release);
        let blob = f.store.commit(s.clone(), upload, wait()).await.unwrap();
        assert_eq!(blob.metadata().image, Some(format));
        assert_eq!(
            f.store.faults().read_bytes.load(Ordering::Acquire),
            12.min(bytes.len()) as u64
        );
        let reader = f.store.reader(s.clone(), blob, wait()).await.unwrap();
        assert_eq!(
            f.store
                .read(ReadAccess::Invocation(s.clone()), reader, wait())
                .await
                .unwrap()
                .bytes,
            bytes
        );
        f.store.close_invocation(s, owner, wait()).await.unwrap();
        f.finish().await;
    }
}

#[tokio::test]
async fn unknown_length_image_upload_cannot_exceed_twenty_mib() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let mut meta = metadata(None);
    meta.kind = BlobKind::Image(ImageFormat::Png);
    meta.content_type = "image/png".into();
    let upload = f
        .store
        .begin(s.clone(), owner.clone(), meta, wait())
        .await
        .unwrap();
    for sequence in 0..320 {
        f.store
            .append(
                s.clone(),
                upload.clone(),
                sequence,
                vec![0; CHUNK_BYTES],
                wait(),
            )
            .await
            .unwrap();
    }
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 20 * 1024 * 1024);
    assert!(matches!(
        f.store
            .append(s.clone(), upload.clone(), 320, vec![0], wait())
            .await,
        Err(BlobError::Quota)
    ));
    assert!(matches!(
        f.store.commit(s.clone(), upload, wait()).await,
        Err(BlobError::Stale)
    ));
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
    f.finish().await;
}

#[tokio::test]
async fn deadline_and_dropped_request_interrupt_signature_read_and_preserve_cleanup_ownership() {
    for deadline in [false, true] {
        let f = Fixture::new();
        let s = scope("alice", "one");
        let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
        let mut meta = metadata(Some(8));
        meta.kind = BlobKind::Image(ImageFormat::Png);
        meta.content_type = "image/png".into();
        let upload = f
            .store
            .begin(s.clone(), owner.clone(), meta, wait())
            .await
            .unwrap();
        f.store
            .append(
                s.clone(),
                upload.clone(),
                0,
                b"\x89PNG\r\n\x1a\n".to_vec(),
                wait(),
            )
            .await
            .unwrap();
        let gate = ReadGate::new(&f.store);
        let mut w = wait();
        if deadline {
            w.deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
        }
        let commit = f.store.commit(s.clone(), upload, w);
        eventually(|| f.store.faults().read_entered.load(Ordering::Acquire)).await;
        if deadline {
            assert!(matches!(commit.await, Err(BlobError::Deadline)));
        } else {
            drop(commit);
        }
        drop(gate);
        f.store.retry_cleanup(wait()).await.unwrap();
        assert_eq!(f.store.usage(wait()).await.unwrap().objects, 0);
        f.store.close_invocation(s, owner, wait()).await.unwrap();
        f.finish().await;
    }
}

#[tokio::test]
async fn principal_and_process_reservations_include_output_and_quarantine() {
    let f = Fixture::new();
    let mut owners = Vec::new();
    let mut uploads = Vec::new();
    for principal in 0..4 {
        for invocation in 0..2 {
            let s = scope(&format!("p{principal}"), &format!("i{invocation}"));
            let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
            for _ in 0..8 {
                uploads.push(
                    f.store
                        .begin(
                            s.clone(),
                            owner.clone(),
                            metadata(Some(64 * 1024 * 1024)),
                            wait(),
                        )
                        .await
                        .unwrap(),
                );
            }
            owners.push((s, owner));
        }
    }
    assert_eq!(
        f.store.usage(wait()).await.unwrap().bytes,
        4 * 1024 * 1024 * 1024
    );
    let s = scope("fresh", "invocation");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    assert!(matches!(
        f.store
            .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
            .await,
        Err(BlobError::Quota)
    ));
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    let s = scope("p0", "third");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    assert!(matches!(
        f.store
            .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
            .await,
        Err(BlobError::Quota)
    ));
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    for (s, owner) in owners {
        f.store.close_invocation(s, owner, wait()).await.unwrap();
    }
    assert_eq!(f.store.usage(wait()).await.unwrap().objects, 0);
    f.finish().await;
}

#[tokio::test]
async fn object_and_reader_counts_are_bounded_and_dropped_handles_cleanup() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let mut blobs = Vec::new();
    for _ in 0..128 {
        blobs.push(f.blob(&s, &owner, b"").await);
    }
    assert!(matches!(
        f.store
            .begin(s.clone(), owner.clone(), metadata(None), wait())
            .await,
        Err(BlobError::Quota)
    ));
    let mut readers = Vec::new();
    for blob in &blobs[..8] {
        for _ in 0..4 {
            readers.push(
                f.store
                    .reader(s.clone(), blob.clone(), wait())
                    .await
                    .unwrap(),
            );
        }
    }
    assert!(matches!(
        f.store.reader(s.clone(), blobs[0].clone(), wait()).await,
        Err(BlobError::Quota)
    ));
    assert!(matches!(
        f.store.reader(s.clone(), blobs[8].clone(), wait()).await,
        Err(BlobError::Quota)
    ));
    readers.clear();
    let reader = f
        .store
        .reader(s.clone(), blobs[8].clone(), wait())
        .await
        .unwrap();
    drop(owner);
    assert!(matches!(
        f.store
            .read(ReadAccess::Invocation(s), reader, wait())
            .await,
        Err(BlobError::Stale)
    ));
    f.store.retry_cleanup(wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().objects, 0);
    f.finish().await;
}

#[tokio::test]
async fn shutdown_is_owned_with_dropped_and_concurrent_waiters_and_two_cleanup_workers() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    for _ in 0..8 {
        f.blob(&s, &owner, b"abc").await;
    }
    let gate = DeleteGate::new(&f.store);
    let shutdown = f.store.shutdown();
    drop(shutdown);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while f.store.faults().deletes_active.load(Ordering::Acquire) != 2 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.store.faults().deletes_peak.load(Ordering::Acquire), 2);
    let a = f.store.shutdown();
    let b = f.store.shutdown();
    drop(gate);
    let (a, b) = tokio::join!(a, b);
    a.unwrap();
    b.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn cancelled_or_dropped_commit_never_becomes_visible_and_shutdown_cancels_signature_read() {
    for shutdown in [false, true] {
        let f = Fixture::new();
        let s = scope("alice", "one");
        let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
        let mut meta = metadata(Some(12));
        meta.kind = BlobKind::Image(ImageFormat::WebP);
        meta.content_type = "image/webp".into();
        let upload = f
            .store
            .begin(s.clone(), owner.clone(), meta, wait())
            .await
            .unwrap();
        f.store
            .append(
                s.clone(),
                upload.clone(),
                0,
                b"RIFF\0\0\0\0WEBP".to_vec(),
                wait(),
            )
            .await
            .unwrap();
        let gate = ReadGate::new(&f.store);
        let w = wait();
        let cancellation = w.cancellation.clone();
        let commit = f.store.commit(s.clone(), upload, w);
        eventually(|| f.store.faults().read_entered.load(Ordering::Acquire)).await;
        if shutdown {
            let shutdown = f.store.shutdown();
            drop(gate);
            assert!(matches!(commit.await, Err(BlobError::Cancelled)));
            shutdown.await.unwrap();
        } else {
            cancellation.cancel();
            drop(commit);
            drop(gate);
            f.store.retry_cleanup(wait()).await.unwrap();
            assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
            f.store.close_invocation(s, owner, wait()).await.unwrap();
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn shutdown_delete_failure_can_be_retried_without_losing_ownership() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    f.blob(&s, &owner, b"abc").await;
    f.store.faults().delete_failures.store(1, Ordering::Release);
    assert_eq!(f.store.shutdown().await, Err(BlobError::Cleanup));
    assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 1);
    f.store.shutdown().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn abandoned_success_replies_revoke_mutation_or_cursor_without_partial_reuse() {
    for operation in ["append", "commit", "transfer", "read"] {
        let f = Fixture::new();
        let s = scope("alice", "one");
        let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
        let mut held_reader = None;
        let mut held_upload = None;
        let pending: BlobFuture<()> = if matches!(operation, "append" | "commit") {
            let upload = f
                .store
                .begin(s.clone(), owner.clone(), metadata(Some(3)), wait())
                .await
                .unwrap();
            if operation == "append" {
                held_upload = Some(upload.clone());
                f.store
                    .append(s.clone(), upload.clone(), 0, b"abc".to_vec(), wait())
            } else {
                f.store
                    .append(s.clone(), upload.clone(), 0, b"abc".to_vec(), wait())
                    .await
                    .unwrap();
                let commit = f.store.commit(s.clone(), upload, wait());
                Box::pin(async move { commit.await.map(|_| ()) })
            }
        } else {
            let blob = f.blob(&s, &owner, b"abc").await;
            if operation == "transfer" {
                let transfer = f.store.transfer_output(s.clone(), blob, wait());
                Box::pin(async move { transfer.await.map(|_| ()) })
            } else {
                let reader = f.store.reader(s.clone(), blob, wait()).await.unwrap();
                held_reader = Some(reader.clone());
                let read = f
                    .store
                    .read(ReadAccess::Invocation(s.clone()), reader, wait());
                Box::pin(async move { read.await.map(|_| ()) })
            }
        };
        // FIFO actor observation proves mutation completed before response abandonment.
        f.store.usage(wait()).await.unwrap();
        drop(pending);
        f.store.retry_cleanup(wait()).await.unwrap();
        if let Some(reader) = held_reader {
            assert!(matches!(
                f.store
                    .read(ReadAccess::Invocation(s.clone()), reader, wait())
                    .await,
                Err(BlobError::Stale)
            ));
            assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 3);
        } else {
            assert_eq!(f.store.usage(wait()).await.unwrap().objects, 0);
        }
        drop(held_upload);
        f.store.close_invocation(s, owner, wait()).await.unwrap();
        f.finish().await;
    }
}

#[tokio::test]
async fn explicit_owner_close_denies_commit_after_signature_read() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let mut meta = metadata(Some(2));
    meta.kind = BlobKind::Image(ImageFormat::Jpeg);
    meta.content_type = "image/jpeg".into();
    let upload = f
        .store
        .begin(s.clone(), owner.clone(), meta, wait())
        .await
        .unwrap();
    f.store
        .append(s.clone(), upload.clone(), 0, b"\xff\xd8".to_vec(), wait())
        .await
        .unwrap();
    let gate = ReadGate::new(&f.store);
    let commit = f.store.commit(s.clone(), upload, wait());
    eventually(|| f.store.faults().read_entered.load(Ordering::Acquire)).await;
    let close = f.store.close_invocation(s, owner, wait());
    drop(gate);
    assert!(matches!(commit.await, Err(BlobError::Stale)));
    close.await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().objects, 0);
    f.finish().await;
}

#[tokio::test]
async fn process_and_principal_object_caps_bound_the_entire_cleanup_queue() {
    let f = Fixture::new();
    let mut owners = Vec::new();
    let mut uploads = Vec::new();
    for principal in 0..4 {
        for invocation in 0..8 {
            let s = scope(&format!("p{principal}"), &format!("i{invocation}"));
            let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
            for _ in 0..128 {
                uploads.push(
                    f.store
                        .begin(s.clone(), owner.clone(), metadata(Some(0)), wait())
                        .await
                        .unwrap(),
                );
            }
            owners.push((s, owner));
        }
        let s = scope(&format!("p{principal}"), "overflow");
        let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
        assert!(matches!(
            f.store
                .begin(s.clone(), owner.clone(), metadata(Some(0)), wait())
                .await,
            Err(BlobError::Quota)
        ));
        f.store.close_invocation(s, owner, wait()).await.unwrap();
    }
    let s = scope("fresh", "overflow");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    assert!(matches!(
        f.store
            .begin(s.clone(), owner.clone(), metadata(Some(0)), wait())
            .await,
        Err(BlobError::Quota)
    ));
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().objects, 4096);
    let gate = DeleteGate::new(&f.store);
    for (_, owner) in owners {
        drop(owner);
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let usage = f.store.usage(wait()).await.unwrap();
        if usage.deleting == 4096 && f.store.faults().deletes_active.load(Ordering::Acquire) == 2 {
            assert_eq!(usage.objects, 4096);
            assert_eq!(usage.bytes, 0);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    drop(gate);
    drop(uploads);
    f.store.retry_cleanup(wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().objects, 0);
    f.finish().await;
}

#[tokio::test]
async fn names_are_metadata_only_and_private_modes_are_enforced() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    for name in [
        "../escape",
        "/absolute",
        "https://example.test/image",
        "archive\\entry",
        "..",
        "a\0b",
    ] {
        let mut meta = metadata(None);
        meta.display_name = name.into();
        assert!(matches!(
            f.store.begin(s.clone(), owner.clone(), meta, wait()).await,
            Err(BlobError::Invalid)
        ));
    }
    let blob = f.blob(&s, &owner, b"inert").await;
    let namespace = std::fs::read_dir(&f.root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::metadata(&namespace).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let entry = std::fs::read_dir(namespace)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert!(Uuid::parse_str(entry.file_name().to_str().unwrap()).is_ok());
    assert_ne!(
        entry.file_name().to_str().unwrap(),
        blob.metadata().display_name
    );
    let file = std::fs::metadata(entry.path()).unwrap();
    assert_eq!(file.mode() & 0o777, 0o600);
    assert_eq!(file.nlink(), 1);
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn real_truncation_is_not_silently_returned_as_short_success() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let blob = f.blob(&s, &owner, b"abcdef").await;
    let namespace = std::fs::read_dir(&f.root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let path = std::fs::read_dir(namespace)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(2)
        .unwrap();
    let reader = f.store.reader(s.clone(), blob, wait()).await.unwrap();
    assert_eq!(
        f.store
            .read(ReadAccess::Invocation(s.clone()), reader, wait())
            .await,
        Err(BlobError::Invalid)
    );
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn unsafe_roots_symlinks_workspace_overlap_and_orphans_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let f = Fixture::new();
    let candidate = f.root.join("candidate");
    let workspace = f.root.join("worker");
    std::fs::create_dir(&candidate).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        FilesystemBlobStore::check_root(&candidate, &workspace),
        Err(BlobError::Denied)
    );
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = f.root.join("symlink");
    symlink(&candidate, &link).unwrap();
    assert!(FilesystemBlobStore::check_root(&link, &workspace).is_err());
    assert_eq!(
        FilesystemBlobStore::check_root(&candidate, &candidate),
        Err(BlobError::Denied)
    );
    assert_eq!(
        FilesystemBlobStore::check_root(&candidate, &f.root),
        Err(BlobError::Denied)
    );
    std::fs::write(candidate.join("orphan"), b"preserve").unwrap();
    assert_eq!(
        FilesystemBlobStore::check_root(&candidate, &workspace),
        Err(BlobError::Cleanup)
    );
    assert_eq!(
        std::fs::read(candidate.join("orphan")).unwrap(),
        b"preserve"
    );
    std::fs::remove_file(candidate.join("orphan")).unwrap();
    FilesystemBlobStore::check_root(&candidate, &workspace).unwrap();
    std::fs::remove_file(link).unwrap();
    std::fs::remove_dir(candidate).unwrap();
    std::fs::remove_dir(workspace).unwrap();
    f.finish().await;
}

#[tokio::test]
async fn pinned_root_does_not_follow_a_replacement_path() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let moved = f.root.with_extension("moved");
    std::fs::rename(&f.root, &moved).unwrap();
    symlink(std::env::current_dir().unwrap(), &f.root).unwrap();
    let blob = f.blob(&s, &owner, b"anchored").await;
    assert_eq!(blob.metadata().bytes, 8);
    let namespace = std::fs::read_dir(&moved)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(std::fs::read_dir(namespace).unwrap().count(), 1);
    std::fs::remove_file(&f.root).unwrap();
    std::fs::rename(moved, &f.root).unwrap();
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn foreign_generation_invocation_kind_and_replayed_commit_are_denied_without_revocation() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let upload = f
        .store
        .begin(s.clone(), owner.clone(), metadata(Some(0)), wait())
        .await
        .unwrap();
    let blob = f
        .store
        .commit(s.clone(), upload.clone(), wait())
        .await
        .unwrap();
    assert!(matches!(
        f.store.commit(s.clone(), upload.clone(), wait()).await,
        Err(BlobError::Stale)
    ));
    assert!(matches!(
        f.store.commit(scope("bob", "one"), upload, wait()).await,
        Err(BlobError::Denied)
    ));
    let mut stale = blob.clone();
    stale.id.instance = Uuid::new_v4();
    assert!(matches!(
        f.store.reader(s.clone(), stale, wait()).await,
        Err(BlobError::Stale)
    ));
    assert!(matches!(
        f.store
            .reader(scope("alice", "two"), blob.clone(), wait())
            .await,
        Err(BlobError::Denied)
    ));
    let wrong_kind = OutputLease {
        id: blob.id,
        metadata: blob.metadata.clone(),
        expires_at_ms: 10,
    };
    assert!(matches!(
        f.store
            .output_reader("alice".into(), wrong_kind, wait())
            .await,
        Err(BlobError::Stale)
    ));
    f.store.reader(s.clone(), blob, wait()).await.unwrap();
    f.store.close_invocation(s, owner, wait()).await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn expiry_during_actual_filesystem_read_returns_no_bytes_and_retains_cleanup_charge() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let blob = f.blob(&s, &owner, b"abc").await;
    let lease = f.store.transfer_output(s, blob, wait()).await.unwrap();
    let reader = f
        .store
        .output_reader("alice".into(), lease.clone(), wait())
        .await
        .unwrap();
    let deletes = DeleteGate::new(&f.store);
    let reads = ReadGate::new(&f.store);
    let pending = f.store.read(
        ReadAccess::Output {
            principal: "alice".into(),
        },
        reader,
        wait(),
    );
    eventually(|| f.store.faults().read_entered.load(Ordering::Acquire)).await;
    assert_eq!(f.store.faults().read_bytes.load(Ordering::Acquire), 3);
    f.clock.0.store(lease.expires_at_ms(), Ordering::Release);
    drop(reads);
    assert!(matches!(pending.await, Err(BlobError::Stale)));
    assert!(matches!(
        f.store.output_reader("alice".into(), lease, wait()).await,
        Err(BlobError::Stale)
    ));
    let usage = f.store.usage(wait()).await.unwrap();
    assert_eq!(
        (usage.bytes, usage.objects, usage.pending_deletions),
        (3, 1, 1)
    );
    drop(deletes);
    f.store.retry_cleanup(wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
    f.finish().await;
}

#[tokio::test]
async fn transferred_output_alone_exhausts_each_byte_budget_until_verified_expiry_deletion() {
    let f = Fixture::new();
    *f.store.faults().byte_limits.lock().unwrap() = Some((8, 16, 32));
    let mut owners = Vec::new();
    let mut leases = Vec::new();
    for (index, s) in [
        scope("alice", "one"),
        scope("alice", "two"),
        scope("bob", "one"),
        scope("bob", "two"),
    ]
    .into_iter()
    .enumerate()
    {
        let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
        let blob = f.blob(&s, &owner, b"12345678").await;
        leases.push(
            f.store
                .transfer_output(s.clone(), blob, wait())
                .await
                .unwrap(),
        );
        if index == 0 {
            assert!(matches!(
                f.store
                    .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
                    .await,
                Err(BlobError::Quota)
            ));
        }
        owners.push((s, owner));
        if index == 1 {
            let s = scope("alice", "three");
            let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
            assert!(matches!(
                f.store
                    .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
                    .await,
                Err(BlobError::Quota)
            ));
            owners.push((s, owner));
        }
    }
    let s = scope("charlie", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    assert!(matches!(
        f.store
            .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
            .await,
        Err(BlobError::Quota)
    ));
    owners.push((s, owner));
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 32);
    let deletes = DeleteGate::new(&f.store);
    f.clock.0.store(OUTPUT_LEASE_MS, Ordering::Release);
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 32);
    for (s, owner) in &owners {
        assert!(matches!(
            f.store
                .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
                .await,
            Err(BlobError::Quota)
        ));
    }
    drop(deletes);
    f.store.retry_cleanup(wait()).await.unwrap();
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
    let mut uploads = Vec::new();
    for principal_invocation in [
        ("alice", "one"),
        ("alice", "three"),
        ("bob", "one"),
        ("charlie", "one"),
    ] {
        let (s, owner) = owners
            .iter()
            .find(|(s, _)| {
                s.principal == principal_invocation.0 && s.invocation == principal_invocation.1
            })
            .unwrap();
        uploads.push(
            f.store
                .begin(s.clone(), owner.clone(), metadata(Some(8)), wait())
                .await
                .unwrap(),
        );
    }
    assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 32);
    drop(uploads);
    drop(leases);
    f.finish().await;
}

#[tokio::test]
async fn saturated_request_queue_recovers_after_cancel_drop_and_drains_on_shutdown() {
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let blob = f.blob(&s, &owner, b"abc").await;
    for shutdown in [false, true] {
        let reader = f
            .store
            .reader(s.clone(), blob.clone(), wait())
            .await
            .unwrap();
        let gate = ReadGate::new(&f.store);
        let read = f
            .store
            .read(ReadAccess::Invocation(s.clone()), reader, wait());
        eventually(|| f.store.faults().read_entered.load(Ordering::Acquire)).await;
        let cancelled = wait();
        let cancelled_reply = f.store.begin(
            s.clone(),
            owner.clone(),
            metadata(Some(1)),
            cancelled.clone(),
        );
        let mut replies: Vec<_> = (0..31)
            .map(|_| {
                f.store
                    .begin(s.clone(), owner.clone(), metadata(Some(1)), wait())
            })
            .collect();
        assert!(matches!(f.store.usage(wait()).await, Err(BlobError::Quota)));
        cancelled.cancellation.cancel();
        assert!(matches!(cancelled_reply.await, Err(BlobError::Cancelled)));
        drop(replies.drain(..15));
        assert!(matches!(f.store.usage(wait()).await, Err(BlobError::Quota)));
        if shutdown {
            let complete = f.store.shutdown();
            drop(read);
            drop(gate);
            for reply in replies {
                assert!(matches!(
                    reply.await,
                    Err(BlobError::Stale | BlobError::Unavailable)
                ));
            }
            complete.await.unwrap();
            assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 0);
        } else {
            drop(gate);
            assert_eq!(read.await.unwrap().bytes, b"abc");
            let mut uploads = Vec::new();
            for reply in replies {
                uploads.push(reply.await.unwrap());
            }
            let usage = f.store.usage(wait()).await.unwrap();
            assert_eq!((usage.objects, usage.bytes), (17, 19));
            drop(uploads);
            f.store.retry_cleanup(wait()).await.unwrap();
            assert_eq!(f.store.usage(wait()).await.unwrap().objects, 1);
            assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 3);
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn last_store_drop_retries_delete_failures_without_an_explicit_shutdown_rescue() {
    let Fixture {
        _serial,
        root,
        store,
        clock,
    } = Fixture::new();
    let s = scope("alice", "one");
    let owner = store.open_invocation(s.clone(), wait()).await.unwrap();
    let upload = store
        .begin(s.clone(), owner.clone(), metadata(Some(3)), wait())
        .await
        .unwrap();
    store
        .append(s.clone(), upload.clone(), 0, b"abc".to_vec(), wait())
        .await
        .unwrap();
    let blob = store.commit(s, upload, wait()).await.unwrap();
    let faults = store.faults().clone();
    faults.delete_failures.store(2, Ordering::Release);
    drop(store);
    eventually(|| std::fs::read_dir(&root).unwrap().next().is_none()).await;
    assert_eq!(faults.delete_failures.load(Ordering::Acquire), 0);
    assert!(faults.deletes_attempted.load(Ordering::Acquire) >= 3);
    // Construction proves that the prior driver released both instance and root locks.
    let mut next = None;
    eventually(|| {
        match FilesystemBlobStore::new(FilesystemConfig {
            private_root: root.clone(),
            workspace_root: std::env::current_dir().unwrap(),
            clock: clock.clone(),
        }) {
            Ok(store) => {
                next = Some(store);
                true
            }
            Err(BlobError::Denied) => false,
            Err(error) => panic!("post-drop construction failed: {error}"),
        }
    })
    .await;
    next.unwrap().shutdown().await.unwrap();
    drop((owner, blob));
    std::fs::remove_dir(root).unwrap();
}

#[tokio::test]
async fn real_unlink_permission_failure_retains_credit_until_permissions_recover() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let s = scope("alice", "one");
    let owner = f.store.open_invocation(s.clone(), wait()).await.unwrap();
    let _blob = f.blob(&s, &owner, b"abc").await;
    let directory = std::fs::read_dir(&f.root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let probe = directory.join("permission-probe");
    std::fs::write(&probe, b"probe").unwrap();
    let restore = RestorePermissions(directory.clone());
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    match std::fs::remove_file(&probe) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            assert!(matches!(
                f.store.close_invocation(s, owner, wait()).await,
                Err(BlobError::Cleanup)
            ));
            assert_eq!(f.store.faults().delete_failures.load(Ordering::Acquire), 0);
            let usage = f.store.usage(wait()).await.unwrap();
            assert_eq!(
                (usage.bytes, usage.objects, usage.pending_deletions),
                (3, 1, 1)
            );
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
            drop(restore);
            std::fs::remove_file(probe).unwrap();
            f.store.retry_cleanup(wait()).await.unwrap();
            assert_eq!(f.store.usage(wait()).await.unwrap().bytes, 0);
            println!("PROVED real filesystem unlink PermissionDenied retained blob credit");
        }
        Ok(()) => {
            drop(restore);
            println!(
                "UNVERIFIED unlink permission fault: process or filesystem bypassed directory write permissions"
            );
        }
        Err(error) => panic!("unexpected permission probe error: {error}"),
    }
    f.finish().await;
}

struct ChildRootLock {
    child: std::process::Child,
    control: PathBuf,
}
impl Drop for ChildRootLock {
    fn drop(&mut self) {
        let _result = std::fs::write(self.control.join("release"), b"release");
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _result = self.child.kill();
        }
        let _result = self.child.wait();
        for name in ["ready", "release"] {
            let _result = std::fs::remove_file(self.control.join(name));
        }
        let _result = std::fs::remove_dir(&self.control);
    }
}

#[test]
fn external_root_lock_child() {
    let Some(root) = std::env::var_os("WALTR_BLOB_TEST_LOCK_ROOT") else {
        return;
    };
    let control = PathBuf::from(std::env::var_os("WALTR_BLOB_TEST_LOCK_CONTROL").unwrap());
    let root = std::fs::File::open(root).unwrap();
    rustix::fs::flock(&root, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    std::fs::write(control.join("ready"), b"ready").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !control.join("release").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        control.join("release").exists(),
        "external root lock timed out"
    );
}

#[tokio::test]
async fn adapter_and_actual_root_lock_contention_release_after_shutdown_and_external_process_exit()
{
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let config = || FilesystemConfig {
        private_root: f.root.clone(),
        workspace_root: std::env::current_dir().unwrap(),
        clock: f.clock.clone(),
    };
    assert!(matches!(
        FilesystemBlobStore::new(config()),
        Err(BlobError::Denied)
    ));
    // This bypasses the process-instance guard and reaches the root flock itself.
    assert!(matches!(
        FilesystemBlobStore::check_root(&f.root, &std::env::current_dir().unwrap()),
        Err(BlobError::Denied)
    ));
    f.store.shutdown().await.unwrap();
    let second = FilesystemBlobStore::new(config()).unwrap();
    second.shutdown().await.unwrap();
    let control = f.root.with_extension("lock-control");
    std::fs::create_dir(&control).unwrap();
    std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700)).unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "routine::blob::tests::external_root_lock_child",
            "--nocapture",
        ])
        .env("WALTR_BLOB_TEST_LOCK_ROOT", &f.root)
        .env("WALTR_BLOB_TEST_LOCK_CONTROL", &control)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildRootLock { child, control };
    eventually(|| child.control.join("ready").exists()).await;
    assert!(matches!(
        FilesystemBlobStore::new(config()),
        Err(BlobError::Denied)
    ));
    std::fs::write(child.control.join("release"), b"release").unwrap();
    let mut status = None;
    eventually(|| {
        status = child.child.try_wait().unwrap();
        status.is_some()
    })
    .await;
    assert!(status.unwrap().success());
    drop(child);
    let third = FilesystemBlobStore::new(config()).unwrap();
    third.shutdown().await.unwrap();
    f.finish().await;
}
