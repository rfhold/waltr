//! App-data is the trusted ancestor boundary; all descendants are opened relative
//! to retained directory descriptors. Same-user hostile ancestors are out of scope.
use super::*;
use rustix::{
    fs::{self, AtFlags, FlockOperation, Mode, OFlags},
    io::Errno,
};
use std::{
    fs::File,
    io::{Read as _, Write as _},
    os::fd::OwnedFd,
    path::Path,
};

const DIRECTORY: u32 = 0o700;
const RECORD: u32 = 0o600;

fn checked(fd: &OwnedFd, directory: bool) -> Result<(), NativeRefreshTokenStoreError> {
    let stat = fs::fstat(fd).map_err(|_| Unavailable)?;
    let kind = fs::FileType::from_raw_mode(stat.st_mode);
    if !private_metadata(
        stat.st_uid,
        u32::from(stat.st_mode),
        kind,
        stat.st_nlink as u64,
        directory,
    ) {
        return Err(Unavailable);
    }
    #[cfg(target_os = "macos")]
    waltr_file_acl::ensure_no_extended_acl(std::os::fd::AsFd::as_fd(fd))
        .map_err(|_| Unavailable)?;
    Ok(())
}

fn private_metadata(
    owner: u32,
    mode: u32,
    kind: fs::FileType,
    links: u64,
    directory: bool,
) -> bool {
    !(owner != rustix::process::geteuid().as_raw()
        || mode & 0o7777 != if directory { DIRECTORY } else { RECORD }
        || kind
            != if directory {
                fs::FileType::Directory
            } else {
                fs::FileType::RegularFile
            }
        || (!directory && links != 1))
}

fn directory(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
) -> Result<OwnedFd, NativeRefreshTokenStoreError> {
    match fs::mkdirat(parent, name, Mode::from_raw_mode(DIRECTORY as _)) {
        Ok(()) => fs::fsync(parent).map_err(|_| Unavailable)?,
        Err(Errno::EXIST) => {}
        Err(_) => return Err(Unavailable),
    }
    let fd = fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| Unavailable)?;
    checked(&fd, true)?;
    Ok(fd)
}

struct Locked {
    dir: OwnedFd,
    _lock: OwnedFd,
}

impl Drop for Locked {
    fn drop(&mut self) {
        // A fork may retain the file description until exec closes its copy.
        // Release the completed operation's lock independently of those copies.
        let _unlock = fs::flock(&self._lock, FlockOperation::Unlock);
    }
}

impl Locked {
    fn open(root: &Path, server: &str) -> Result<Self, NativeRefreshTokenStoreError> {
        let parent = root.parent().ok_or(Unavailable)?;
        let name = root.file_name().ok_or(Unavailable)?;
        // The native app-data ancestor is supplied by composition, never IPC.
        let parent = fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Unavailable)?;
        let root = directory(&parent, name)?;
        let dir = directory(&root, std::ffi::OsStr::new(server))?;
        let lock = fs::openat(
            &dir,
            "lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(RECORD as _),
        )
        .map_err(|_| Unavailable)?;
        checked(&lock, false)?;
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|_| Unavailable)?;
        fs::fsync(&dir).map_err(|_| Unavailable)?;
        Ok(Self { dir, _lock: lock })
    }

    fn read(
        &self,
        name: &str,
        max: usize,
    ) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        let fd = match fs::openat(
            &self.dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err(Unavailable),
        };
        checked(&fd, false)?;
        let mut bytes = Vec::new();
        File::from(fd)
            .take((max + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Unavailable)?;
        if bytes.len() > max {
            return Err(Malformed);
        }
        Ok(Some(bytes))
    }

    fn generation(&self) -> Result<u64, NativeRefreshTokenStoreError> {
        match self.read("generation", 8)? {
            None => Ok(0),
            Some(bytes) => Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| Malformed)?)),
        }
    }

    fn validate_existing(&self, name: &str) -> Result<(), NativeRefreshTokenStoreError> {
        match fs::openat(
            &self.dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => checked(&fd, false),
            Err(Errno::NOENT) => Ok(()),
            Err(_) => Err(Unavailable),
        }
    }

    fn replace(&self, name: &str, bytes: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
        self.replace_before_commit(name, bytes, || Ok(()))
    }

    fn replace_before_commit(
        &self,
        name: &str,
        bytes: &[u8],
        before_commit: impl FnOnce() -> Result<(), NativeRefreshTokenStoreError>,
    ) -> Result<(), NativeRefreshTokenStoreError> {
        self.validate_existing(name)?;
        let temporary = format!("tmp-{:032x}", rand::random::<u128>());
        let fd = fs::openat(
            &self.dir,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(RECORD as _),
        )
        .map_err(|_| Unavailable)?;
        let result = (|| {
            checked(&fd, false)?;
            let mut file = File::from(fd);
            file.write_all(bytes).map_err(|_| Unavailable)?;
            file.sync_all().map_err(|_| Unavailable)?;
            before_commit()?;
            fs::renameat(&self.dir, temporary.as_str(), &self.dir, name)
                .map_err(|_| Unavailable)?;
            fs::fsync(&self.dir).map_err(|_| Unavailable)
        })();
        if result.is_err() {
            let _ = fs::unlinkat(&self.dir, temporary.as_str(), AtFlags::empty());
        }
        result
    }

    fn delete(&self, name: &str) -> Result<(), NativeRefreshTokenStoreError> {
        self.validate_existing(name)?;
        match fs::unlinkat(&self.dir, name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(_) => return Err(Unavailable),
        }
        fs::fsync(&self.dir).map_err(|_| Unavailable)
    }
}

pub(super) fn generation(root: &Path, server: &str) -> Result<u64, NativeRefreshTokenStoreError> {
    Locked::open(root, server)?.generation()
}

fn current(store: &CredentialStore) -> Result<Locked, NativeRefreshTokenStoreError> {
    let locked = Locked::open(&store.root, &store.server)?;
    if locked.generation()? != store.durable_generation {
        return Err(Unavailable);
    }
    Ok(locked)
}

pub(super) fn load(
    store: &CredentialStore,
) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
    current(store)?.read(&store.account, MAX_TOKEN_BYTES)
}

pub(super) fn store(
    store: &CredentialStore,
    token: &[u8],
) -> Result<(), NativeRefreshTokenStoreError> {
    current(store)?.replace(&store.account, token)
}

pub(super) fn delete(store: &CredentialStore) -> Result<(), NativeRefreshTokenStoreError> {
    current(store)?.delete(&store.account)
}

pub(super) fn delete_server(root: &Path, server: &str) -> Result<(), NativeRefreshTokenStoreError> {
    let locked = Locked::open(root, server)?;
    let next = locked.generation()?.checked_add(1).ok_or(Unavailable)?;
    // Never roll back a committed revocation, including when later deletion fails.
    locked.replace("generation", &next.to_be_bytes())?;
    let entries = fs::Dir::read_from(&locked.dir).map_err(|_| Unavailable)?;
    for entry in entries {
        let entry = entry.map_err(|_| Unavailable)?;
        let name = entry.file_name().to_str().map_err(|_| Unavailable)?;
        if matches!(name, "." | ".." | "lock" | "generation") {
            continue;
        }
        let account = name.len() == 64
            && name
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        let staging = name.strip_prefix("tmp-").is_some_and(|s| {
            s.len() == 32
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        });
        if !account && !staging {
            return Err(Unavailable);
        }
        locked.delete(name)?;
    }
    // Verify completion without retrying deletion or following unknown objects.
    for entry in fs::Dir::read_from(&locked.dir).map_err(|_| Unavailable)? {
        let entry = entry.map_err(|_| Unavailable)?;
        let name = entry.file_name().to_str().map_err(|_| Unavailable)?;
        if !matches!(name, "." | ".." | "lock" | "generation") {
            return Err(Unavailable);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("waltr-localfile-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("credentials")
        }
        fn store(&self, endpoint: &str) -> Arc<CredentialStore> {
            CredentialStore::at(
                self.path(),
                CredentialIdentity {
                    server_id: "localfile-test",
                    endpoint,
                    issuer: "https://issuer.example/",
                    client_id: "client",
                    scopes: &["openid".into()],
                    audience: None,
                },
            )
            .unwrap()
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }
    fn record(store: &CredentialStore) -> PathBuf {
        store.root.join(&store.server).join(&store.account)
    }

    #[cfg(target_os = "macos")]
    fn grant_acl(root: &Root, path: &Path, entry: &str) {
        assert!(path.starts_with(&root.0));
        assert!(
            std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg(entry)
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn existing_record_acl_blocks_read_and_replacement() {
        let root = Root::new();
        let a = root.store("https://a.example");
        store(&a, b"fixture").unwrap();
        grant_acl(&root, &record(&a), "everyone allow read");
        assert_eq!(load(&a), Err(Unavailable));
        assert_eq!(store(&a, b"replacement"), Err(Unavailable));
        assert_eq!(std::fs::read(record(&a)).unwrap(), b"fixture");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn inherited_directory_acl_blocks_operations() {
        let root = Root::new();
        let a = root.store("https://a.example");
        grant_acl(
            &root,
            &a.root,
            "everyone allow read,write,execute,file_inherit,directory_inherit",
        );
        assert_eq!(store(&a, b"fixture"), Err(Unavailable));
        assert!(!record(&a).exists());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn inherited_staging_acl_is_rejected_before_write() {
        let root = Root::new();
        let a = root.store("https://a.example");
        let locked = Locked::open(&a.root, &a.server).unwrap();
        grant_acl(
            &root,
            &a.root.join(&a.server),
            "everyone allow read,write,execute,file_inherit,directory_inherit",
        );
        assert_eq!(locked.replace(&a.account, b"fixture"), Err(Unavailable));
        assert!(!record(&a).exists());
        assert!(
            !std::fs::read_dir(a.root.join(&a.server))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(b"tmp-"))
        );
    }

    #[test]
    fn private_atomic_rotation_and_identity_isolation() {
        let root = Root::new();
        let a = root.store("https://a.example");
        let b = root.store("https://b.example");
        assert_eq!(load(&a).unwrap(), None);
        store(&a, b"first").unwrap();
        store(&a, b"second").unwrap();
        assert_eq!(load(&a).unwrap(), Some(b"second".to_vec()));
        assert_eq!(load(&b).unwrap(), None);
        for path in [a.root.clone(), a.root.join(&a.server)] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                DIRECTORY
            );
        }
        for name in [&a.account, "lock"] {
            assert_eq!(
                std::fs::metadata(a.root.join(&a.server).join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                RECORD
            );
        }
        delete(&a).unwrap();
        assert_eq!(load(&a).unwrap(), None);
    }

    #[test]
    fn foreign_owner_and_precommit_failure_are_rejected() {
        assert!(!private_metadata(
            rustix::process::geteuid().as_raw().wrapping_add(1),
            RECORD,
            fs::FileType::RegularFile,
            1,
            false
        ));
        let root = Root::new();
        let a = root.store("https://a.example");
        store(&a, b"old").unwrap();
        let locked = Locked::open(&a.root, &a.server).unwrap();
        assert_eq!(
            locked.replace_before_commit(&a.account, b"new", || Err(Unavailable)),
            Err(Unavailable)
        );
        drop(locked);
        assert_eq!(load(&a).unwrap(), Some(b"old".to_vec()));
        let names: Vec<_> = std::fs::read_dir(a.root.join(&a.server))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            !names
                .iter()
                .any(|n| n.to_string_lossy().starts_with("tmp-"))
        );
    }

    #[test]
    fn lock_contention_fails_without_waiting_or_retrying() {
        let root = Root::new();
        let a = root.store("https://locked.example");
        let _locked = Locked::open(&a.root, &a.server).unwrap();
        assert_eq!(generation(&a.root, &a.server), Err(Unavailable));
    }

    #[test]
    fn inherited_descriptor_cannot_extend_completed_operation_lock() {
        let root = Root::new();
        let a = root.store("https://locked.example");
        let locked = Locked::open(&a.root, &a.server).unwrap();
        // A fork can retain this file description until its CLOEXEC close.
        let inherited = rustix::io::dup(&locked._lock).unwrap();
        drop(locked);
        assert_eq!(generation(&a.root, &a.server), Ok(0));
        drop(inherited);
    }

    #[test]
    fn rejects_existing_permissions_links_and_nonregular_records() {
        let root = Root::new();
        std::fs::create_dir(root.path()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(generation(&root.path(), "server").is_err());
        std::fs::remove_dir(root.path()).unwrap();
        symlink(&root.0, root.path()).unwrap();
        assert!(generation(&root.path(), "server").is_err());
        std::fs::remove_file(root.path()).unwrap();
        let a = root.store("https://a.example");
        let target = root.0.join("sentinel");
        std::fs::write(&target, b"untouched").unwrap();
        symlink(&target, record(&a)).unwrap();
        assert_eq!(load(&a), Err(Unavailable));
        assert_eq!(store(&a, b"replacement"), Err(Unavailable));
        assert_eq!(delete(&a), Err(Unavailable));
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        std::fs::remove_file(record(&a)).unwrap();
        store(&a, b"linked").unwrap();
        let hardlink = root.0.join("hardlink");
        std::fs::hard_link(record(&a), &hardlink).unwrap();
        assert_eq!(load(&a), Err(Unavailable));
        assert_eq!(delete(&a), Err(Unavailable));
        std::fs::remove_file(&hardlink).unwrap();
        std::fs::remove_file(record(&a)).unwrap();
        std::fs::create_dir(record(&a)).unwrap();
        assert_eq!(load(&a), Err(Unavailable));
        std::fs::remove_dir(record(&a)).unwrap();
        store(&a, b"old").unwrap();
        std::fs::set_permissions(record(&a), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store(&a, b"new"), Err(Unavailable));
        assert_eq!(std::fs::read(record(&a)).unwrap(), b"old");
    }

    #[test]
    fn bounded_reads_and_failed_revocation_preserve_data() {
        let root = Root::new();
        let a = root.store("https://a.example");
        store(&a, &vec![1; MAX_TOKEN_BYTES + 1]).unwrap();
        assert_eq!(load(&a), Err(Malformed));
        store(&a, b"old").unwrap();
        let locked = Locked::open(&a.root, &a.server).unwrap();
        locked.replace("generation", b"bad").unwrap();
        drop(locked);
        assert_eq!(delete_server(&a.root, &a.server), Err(Malformed));
        assert_eq!(std::fs::read(record(&a)).unwrap(), b"old");
    }

    #[test]
    fn durable_revocation_survives_later_deletion_failure() {
        let root = Root::new();
        let a = root.store("https://a.example");
        let b = root.store("https://b.example");
        let other = CredentialStore::at(
            root.path(),
            CredentialIdentity {
                server_id: "unrelated-server",
                endpoint: "https://a.example",
                issuer: "https://issuer.example/",
                client_id: "client",
                scopes: &["openid".into()],
                audience: None,
            },
        )
        .unwrap();
        store(&other, b"unrelated").unwrap();
        store(&a, b"a").unwrap();
        store(&b, b"b").unwrap();
        std::fs::set_permissions(record(&b), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(delete_server(&a.root, &a.server), Err(Unavailable));
        assert_eq!(store(&a, b"detached"), Err(Unavailable));
        assert_eq!(load(&b), Err(Unavailable));
        assert_eq!(generation(&a.root, &a.server).unwrap(), 1);
        std::fs::set_permissions(record(&b), std::fs::Permissions::from_mode(0o600)).unwrap();
        delete_server(&a.root, &a.server).unwrap();
        assert!(!record(&a).exists());
        assert!(!record(&b).exists());
        assert_eq!(load(&other).unwrap(), Some(b"unrelated".to_vec()));
        let fresh = root.store("https://a.example");
        store(&fresh, b"new-sign-in").unwrap();
    }

    #[tokio::test]
    async fn failed_durable_generation_still_fences_local_handles() {
        let root = Root::new();
        let a = root.store("https://failure.example");
        store(&a, b"preserved").unwrap();
        let locked = Locked::open(&a.root, &a.server).unwrap();
        locked.replace("generation", b"malformed").unwrap();
        drop(locked);
        assert_eq!(
            CredentialStore::delete_server(root.path(), "localfile-test").await,
            Err(Malformed)
        );
        assert_eq!(a.with_current(|| Ok(())), Err(Unavailable));
        assert_eq!(std::fs::read(record(&a)).unwrap(), b"preserved");
    }

    #[tokio::test]
    async fn malformed_file_contents_and_oversized_records_stay_bounded() {
        let root = Root::new();
        let a = root.store("https://malformed.example");
        for bytes in [vec![], vec![b'x'; MAX_TOKEN_BYTES + 1]] {
            store(&a, &bytes).unwrap();
            assert_eq!(
                NativeRefreshTokenStore::load(a.as_ref()).await,
                Err(Malformed)
            );
        }
        store(&a, b"preserved").unwrap();
        assert_eq!(
            NativeRefreshTokenStore::store(a.as_ref(), b"").await,
            Err(Malformed)
        );
        assert_eq!(load(&a).unwrap(), Some(b"preserved".to_vec()));
    }

    #[tokio::test]
    async fn queued_blocking_write_cannot_recreate_revoked_record() {
        let root = Root::new();
        let a = root.store("https://detached.example");
        store(&a, b"initial").unwrap();
        let old = a.clone();
        let task = {
            let _guard = a.fence.io.lock().unwrap();
            let task = tokio::task::spawn_blocking(move || {
                old.with_current(|| super::store(&old, b"late"))
            });
            a.fence.generation.fetch_add(1, Ordering::SeqCst);
            delete_server(&a.root, &a.server).unwrap();
            task
        };
        assert_eq!(task.await.unwrap(), Err(Unavailable));
        assert!(!record(&a).exists());
    }

    #[test]
    fn cross_process_revocation_worker() {
        let Some(root) = std::env::var_os("WALTR_TEST_CREDENTIAL_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let store = CredentialStore::at(
            root.join("credentials"),
            CredentialIdentity {
                server_id: "localfile-test",
                endpoint: "https://a.example",
                issuer: "https://issuer.example/",
                client_id: "client",
                scopes: &["openid".into()],
                audience: None,
            },
        )
        .unwrap();
        std::fs::write(root.join("ready"), []).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.join("proceed").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(super::store(&store, b"late-write"), Err(Unavailable));
    }

    #[test]
    fn cross_process_old_handles_cannot_recreate_deleted_tokens() {
        let root = Root::new();
        let a = root.store("https://a.example");
        store(&a, b"initial").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "credential_store::platform::tests::cross_process_revocation_worker",
                "--nocapture",
            ])
            .env("WALTR_TEST_CREDENTIAL_ROOT", &root.0)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.0.join("ready").exists() {
            if std::time::Instant::now() >= deadline {
                drop(child.kill());
                panic!("worker did not become ready");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        delete_server(&a.root, &a.server).unwrap();
        std::fs::write(root.0.join("proceed"), []).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(!record(&a).exists());
    }
}
