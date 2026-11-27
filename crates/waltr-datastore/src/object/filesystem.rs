use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self, AtFlags, Dir, FileType, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use tokio::io::AsyncRead;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::{JoinHandle, spawn_blocking};
use tokio::time::timeout;
use uuid::Uuid;

use super::{
    Actor, Attribution, Error, MAX_OBJECT_BYTES, Namespace, OPERATION_TIME, ObjectInfo,
    ObjectOperation, RECEIPT_BYTES, RECEIPT_VERSION, RECEIPTS, Reader, Receipt, SCAN_LIMIT,
    object_info, object_key, receipt_name, spool,
};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const NEW_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Clone)]
pub(super) struct Filesystem {
    root: Arc<OwnedFd>,
}

impl Filesystem {
    pub(super) fn open(path: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(path).map_err(|_| Error::Unavailable)?;
        let root =
            Arc::new(fs::open(path, DIR_FLAGS, Mode::empty()).map_err(|_| Error::Unavailable)?);
        Ok(Self { root })
    }

    pub(super) async fn stat(&self, ns: &Namespace, key: &str) -> Result<ObjectInfo, Error> {
        let (info, _) = self.read_file(ns, key, false).await?;
        Ok(info)
    }

    pub(super) async fn read(
        &self,
        ns: &Namespace,
        key: &str,
    ) -> Result<(ObjectInfo, Reader), Error> {
        let (info, file) = self.read_file(ns, key, true).await?;
        Ok((info, Box::new(tokio::fs::File::from_std(file))))
    }

    async fn read_file(
        &self,
        ns: &Namespace,
        key: &str,
        for_read: bool,
    ) -> Result<(ObjectInfo, File), Error> {
        let backend = self.clone();
        let ns = ns.clone();
        let key = key.to_owned();
        job(spawn_blocking(move || {
            backend.open_file(&ns, &key, for_read)
        }))
        .await
    }

    fn open_file(
        &self,
        ns: &Namespace,
        key: &str,
        for_read: bool,
    ) -> Result<(ObjectInfo, File), Error> {
        let data = directories(&*self.root, ns.as_str(), false)?;
        let (dir, name) = parent(&data, key, false)?;
        let file = open_regular(&dir, name)?;
        let receipt = self
            .control(ns, false)
            .ok()
            .and_then(|dir| read_receipt(&dir, key));
        let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
        let marker = identity(&file, &metadata);
        let mut info = object_info(key, metadata.len(), marker.as_deref(), receipt);
        if for_read && info.attribution.last_edited_by.is_some() {
            if info.size > MAX_OBJECT_BYTES as u64 {
                return Err(Error::Limit);
            }
            // An outside writer can edit an open inode. Serve staged bytes when claiming authorship.
            let mut snapshot = tempfile::tempfile().map_err(|_| Error::Unavailable)?;
            let size = std::io::copy(
                &mut (&file).take(MAX_OBJECT_BYTES as u64 + 1),
                &mut snapshot,
            )
            .map_err(|_| Error::Unavailable)?;
            if size > MAX_OBJECT_BYTES as u64 {
                return Err(Error::Limit);
            }
            let after = file.metadata().map_err(|_| Error::Unavailable)?;
            if size != info.size || identity(&file, &after) != marker {
                info.attribution = Attribution::default();
            }
            info.size = size;
            snapshot.rewind().map_err(|_| Error::Unavailable)?;
            return Ok((info, snapshot));
        }
        Ok((info, file))
    }

    pub(super) async fn list(
        &self,
        ns: &Namespace,
        prefix: Option<&str>,
        after: Option<&str>,
        count: usize,
    ) -> Result<Vec<ObjectInfo>, Error> {
        let backend = self.clone();
        let ns = ns.clone();
        let prefix = prefix.map(str::to_owned);
        let after = after.map(str::to_owned);
        job(spawn_blocking(move || {
            let data = match directories(&*backend.root, ns.as_str(), false) {
                Ok(data) => data,
                Err(Error::NotFound) => return Ok(Vec::new()),
                Err(err) => return Err(err),
            };
            let mut scan = Scan {
                after: after.as_deref(),
                count,
                seen: 0,
                keys: BTreeSet::new(),
            };
            if let Some(prefix) = prefix.as_deref() {
                match directories(&data, prefix, false) {
                    Ok(dir) => scan.directory(&dir, prefix)?,
                    Err(Error::NotFound) => scan.key(prefix.to_owned()),
                    Err(err) => return Err(err),
                }
            } else {
                scan.directory(&data, "")?;
            }
            scan.keys
                .into_iter()
                .filter_map(|key| match backend.open_file(&ns, &key, false) {
                    Ok((info, _)) => Some(Ok(info)),
                    Err(Error::NotFound) => None,
                    Err(err) => Some(Err(err)),
                })
                .collect()
        }))
        .await
    }

    pub(super) async fn put(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        body: impl AsyncRead + Unpin,
        operation: ObjectOperation,
        permit: OwnedSemaphorePermit,
    ) -> Result<ObjectInfo, Error> {
        let backend = self.clone();
        let ns = ns.clone();
        let ns_for_stage = ns.clone();
        let (scratch, file) = job(spawn_blocking(move || {
            let control = backend.control(&ns_for_stage, true)?;
            Scratch::new(Arc::new(control))
        }))
        .await?;
        let mut file = tokio::fs::File::from_std(file);
        let size = spool(body, &mut file).await?;
        file.sync_all().await.map_err(|_| Error::Unavailable)?;

        let file = file.into_std().await;
        let backend = self.clone();
        let key = key.to_owned();
        let actor = actor.clone();
        mutation(spawn_blocking(move || {
            let _permit = permit;
            backend.publish(&ns, &key, &actor, operation, scratch, &file, size)
        }))
        .await
    }

    fn publish(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        operation: ObjectOperation,
        scratch: Scratch,
        file: &File,
        size: u64,
    ) -> Result<ObjectInfo, Error> {
        lock(&scratch.dir)?;
        let creating = operation == ObjectOperation::Create;
        let result = (|| {
            let data = directories(&*self.root, ns.as_str(), creating)?;
            parent(&data, key, creating)
        })();
        let (dir, name) = if creating {
            result.map_err(|err| {
                if err == Error::NotFound {
                    Error::Conflict
                } else {
                    err
                }
            })?
        } else {
            result?
        };
        let previous = if creating {
            Attribution::default()
        } else {
            describe(
                key,
                &open_regular(&dir, name)?,
                read_receipt(&scratch.dir, key),
            )?
            .attribution
        };
        let token = Uuid::new_v4().to_string();
        let tagged = add_tag(file, &token);
        let before = file.metadata().map_err(|_| Error::Unavailable)?;
        file.sync_all().map_err(|_| Error::Unavailable)?;
        if creating {
            create_only(&scratch.dir, &scratch.name, &dir, name)?;
        } else {
            fs::renameat(&*scratch.dir, &scratch.name, &dir, name).map_err(write_error)?;
        }

        let identity = tagged
            .then(|| {
                let after = file.metadata().ok()?;
                same_payload(&before, &after)
                    .then(|| stamp(&after, &token))
                    .flatten()
            })
            .flatten();
        fs::fsync(&dir).map_err(|_| Error::Uncertain)?;
        let receipt = Receipt::new(operation, actor, previous, identity.clone());
        write_receipt(&scratch.dir, key, &receipt).map_err(|_| Error::Uncertain)?;
        Ok(object_info(key, size, identity.as_deref(), Some(receipt)))
    }

    pub(super) async fn delete(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), Error> {
        let backend = self.clone();
        let ns = ns.clone();
        let key = key.to_owned();
        let actor = actor.clone();
        mutation(spawn_blocking(move || {
            let _permit = permit;
            let control = Arc::new(backend.control(&ns, true)?);
            lock(&*control)?;
            let data = directories(&*backend.root, ns.as_str(), false)?;
            let (dir, name) = parent(&data, &key, false)?;
            let previous = describe(
                &key,
                &open_regular(&dir, name)?,
                read_receipt(&*control, &key),
            )?;
            fs::unlinkat(&dir, name, AtFlags::empty()).map_err(write_error)?;
            fs::fsync(&dir).map_err(|_| Error::Uncertain)?;
            let receipt = Receipt::new(ObjectOperation::Delete, &actor, previous.attribution, None);
            write_receipt(&control, &key, &receipt).map_err(|_| Error::Uncertain)?;
            Ok(())
        }))
        .await
    }

    fn control(&self, ns: &Namespace, create: bool) -> Result<OwnedFd, Error> {
        directories(
            &*self.root,
            &format!("{RECEIPTS}/{RECEIPT_VERSION}/{}", ns.as_str()),
            create,
        )
    }
}

async fn job<T>(handle: JoinHandle<Result<T, Error>>) -> Result<T, Error> {
    handle.await.map_err(|_| Error::Unavailable)?
}

async fn mutation<T>(handle: JoinHandle<Result<T, Error>>) -> Result<T, Error> {
    timeout(OPERATION_TIME, handle)
        .await
        .map_err(|_| Error::Uncertain)?
        .map_err(|_| Error::Uncertain)?
}

fn lock(dir: impl AsFd) -> Result<(), Error> {
    fs::flock(dir, FlockOperation::NonBlockingLockExclusive).map_err(|err| {
        if err == Errno::WOULDBLOCK {
            Error::Conflict
        } else {
            Error::Unavailable
        }
    })
}

fn directories(root: impl AsFd, path: &str, create: bool) -> Result<OwnedFd, Error> {
    let mut dir = rustix::io::dup(root).map_err(|_| Error::Unavailable)?;
    for component in path.split('/').filter(|part| !part.is_empty()) {
        dir = open_dir(&dir, component, create)?;
    }
    Ok(dir)
}

fn parent<'a>(root: impl AsFd, key: &'a str, create: bool) -> Result<(OwnedFd, &'a str), Error> {
    let (path, name) = key.rsplit_once('/').unwrap_or(("", key));
    Ok((directories(root, path, create)?, name))
}

fn open_dir(dir: impl AsFd, name: &str, create: bool) -> Result<OwnedFd, Error> {
    match fs::openat(&dir, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => {
            exact(&dir, name)?;
            Ok(fd)
        }
        Err(Errno::NOENT) if create => {
            match fs::mkdirat(&dir, name, Mode::RWXU) {
                Ok(()) => fs::fsync(&dir).map_err(|_| Error::Unavailable)?,
                Err(Errno::EXIST) => {}
                Err(err) => return Err(write_error(err)),
            }
            open_dir(dir, name, false)
        }
        Err(err) => Err(read_error(err)),
    }
}

fn exact(dir: impl AsFd, name: &str) -> Result<(), Error> {
    // Case-folding and normalization aliases must not reopen a different logical name.
    for (i, entry) in Dir::read_from(dir)
        .map_err(|_| Error::Unavailable)?
        .enumerate()
    {
        if i >= SCAN_LIMIT {
            return Err(Error::Limit);
        }
        if entry
            .map_err(|_| Error::Unavailable)?
            .file_name()
            .to_bytes()
            == name.as_bytes()
        {
            return Ok(());
        }
    }
    Err(Error::NotFound)
}

fn open_regular(dir: impl AsFd, name: &str) -> Result<File, Error> {
    let metadata = fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(read_error)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(Error::NotFound);
    }
    let file = File::from(fs::openat(&dir, name, READ_FLAGS, Mode::empty()).map_err(read_error)?);
    if !file.metadata().map_err(|_| Error::Unavailable)?.is_file() {
        return Err(Error::NotFound);
    }
    exact(dir, name)?;
    Ok(file)
}

fn read_receipt(dir: impl AsFd, key: &str) -> Option<Receipt> {
    let file = open_regular(dir, &receipt_name(key)).ok()?;
    let mut bytes = Vec::new();
    file.take(RECEIPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Receipt::parse(&bytes)
}

fn write_receipt(dir: &Arc<OwnedFd>, key: &str, receipt: &Receipt) -> Result<(), Error> {
    let (scratch, mut file) = Scratch::new(dir.clone())?;
    file.write_all(&receipt.serialize()?)
        .map_err(|_| Error::Unavailable)?;
    file.sync_all().map_err(|_| Error::Unavailable)?;
    fs::renameat(&**dir, &scratch.name, &**dir, receipt_name(key)).map_err(write_error)?;
    fs::fsync(&**dir).map_err(|_| Error::Unavailable)
}

struct Scratch {
    dir: Arc<OwnedFd>,
    name: String,
}

impl Scratch {
    fn new(dir: Arc<OwnedFd>) -> Result<(Self, File), Error> {
        let name = format!(".pending-{}", Uuid::new_v4());
        let fd =
            fs::openat(&*dir, &name, NEW_FLAGS, Mode::RUSR | Mode::WUSR).map_err(write_error)?;
        Ok((Self { dir, name }, File::from(fd)))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::unlinkat(&*self.dir, &self.name, AtFlags::empty());
    }
}

fn create_only(from: &OwnedFd, source: &str, to: &OwnedFd, name: &str) -> Result<(), Error> {
    #[cfg(any(target_vendor = "apple", target_os = "android", target_os = "linux"))]
    match fs::renameat_with(from, source, to, name, fs::RenameFlags::NOREPLACE) {
        Ok(()) => return Ok(()),
        Err(Errno::NOSYS | Errno::INVAL | Errno::OPNOTSUPP) => {}
        Err(err) => return Err(write_error(err)),
    }
    fs::linkat(from, source, to, name, AtFlags::empty()).map_err(write_error)?;
    fs::unlinkat(from, source, AtFlags::empty()).map_err(|_| Error::Uncertain)
}

#[cfg(any(target_vendor = "apple", target_os = "android", target_os = "linux"))]
const FILE_TAG: &str = "user.waltr.datastore-object";

fn add_tag(file: &File, token: &str) -> bool {
    #[cfg(any(target_vendor = "apple", target_os = "android", target_os = "linux"))]
    {
        fs::fsetxattr(file, FILE_TAG, token.as_bytes(), fs::XattrFlags::CREATE).is_ok()
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "android", target_os = "linux")))]
    {
        let _ = (file, token);
        false
    }
}

fn identity(file: &File, metadata: &Metadata) -> Option<String> {
    #[cfg(any(target_vendor = "apple", target_os = "android", target_os = "linux"))]
    {
        let mut bytes = [0; 36];
        let size = fs::fgetxattr(file, FILE_TAG, &mut bytes[..]).ok()?;
        let tag = std::str::from_utf8(bytes.get(..size)?).ok()?;
        Uuid::parse_str(tag).ok()?;
        stamp(metadata, tag)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "android", target_os = "linux")))]
    {
        let _ = (file, metadata);
        None
    }
}

fn stamp(meta: &Metadata, tag: &str) -> Option<String> {
    // A copied xattr, another hard link, or a reused inode is not proof of continuity.
    if meta.nlink() != 1 {
        return None;
    }
    let born = meta
        .created()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!(
        "{tag}:{}:{}:{born}:{}:{}:{}:{}:{}",
        meta.dev(),
        meta.ino(),
        meta.ctime(),
        meta.ctime_nsec(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.len(),
    ))
}

fn same_payload(before: &Metadata, after: &Metadata) -> bool {
    before.ino() == after.ino()
        && before.dev() == after.dev()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
}

fn describe(key: &str, file: &File, receipt: Option<Receipt>) -> Result<ObjectInfo, Error> {
    let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
    Ok(object_info(
        key,
        metadata.len(),
        identity(file, &metadata).as_deref(),
        receipt,
    ))
}

fn read_error(err: Errno) -> Error {
    match err {
        Errno::NOENT | Errno::NOTDIR | Errno::LOOP => Error::NotFound,
        _ => Error::Unavailable,
    }
}

fn write_error(err: Errno) -> Error {
    match err {
        Errno::EXIST | Errno::ISDIR | Errno::NOTEMPTY | Errno::NOTDIR | Errno::LOOP => {
            Error::Conflict
        }
        Errno::NOENT => Error::NotFound,
        _ => Error::Unavailable,
    }
}

struct Scan<'a> {
    after: Option<&'a str>,
    count: usize,
    seen: usize,
    keys: BTreeSet<String>,
}

impl Scan<'_> {
    fn key(&mut self, key: String) {
        if self.after.is_none_or(|after| key.as_str() > after) {
            self.keys.insert(key);
            if self.keys.len() > self.count {
                self.keys.pop_last();
            }
        }
    }

    fn directory(&mut self, dir: &OwnedFd, prefix: &str) -> Result<(), Error> {
        for entry in Dir::read_from(dir).map_err(|_| Error::Unavailable)? {
            self.seen += 1;
            if self.seen > SCAN_LIMIT {
                return Err(Error::Limit);
            }
            let entry = entry.map_err(|_| Error::Unavailable)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            let key = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if object_key(&key).is_err() {
                continue;
            }
            let metadata = match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(metadata) => metadata,
                Err(Errno::NOENT) => continue,
                Err(_) => return Err(Error::Unavailable),
            };
            match FileType::from_raw_mode(metadata.st_mode) {
                FileType::RegularFile => self.key(key),
                FileType::Directory => {
                    let stem = format!("{key}/");
                    if self
                        .after
                        .is_some_and(|after| stem.as_str() < after && !after.starts_with(&stem))
                    {
                        continue;
                    }
                    match fs::openat(dir, name, DIR_FLAGS, Mode::empty()) {
                        Ok(child) => self.directory(&child, &key)?,
                        Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => {}
                        Err(_) => return Err(Error::Unavailable),
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
