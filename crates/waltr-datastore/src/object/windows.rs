use std::collections::BTreeSet;
use std::fs::{File, Metadata, TryLockError};
use std::io::{self, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt as _};
use std::path::Path;
use std::sync::Arc;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, MetadataExt as _, OpenOptions, OpenOptionsExt as _};
use tokio::io::AsyncRead;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::{JoinHandle, spawn_blocking};
use tokio::time::timeout;
use uuid::Uuid;

use super::{
    Actor, Attribution, Error, Namespace, OPERATION_TIME, ObjectInfo, ObjectOperation,
    RECEIPT_BYTES, RECEIPT_VERSION, RECEIPTS, Reader, Receipt, SCAN_LIMIT, object_info, object_key,
    receipt_name, spool,
};

// Windows FILE_* constants for open options and metadata.
const SHARE_READ: u32 = 0x0000_0001;
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[derive(Clone)]
pub(super) struct Filesystem {
    root: Directory,
}

impl Filesystem {
    pub(super) fn open(path: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(path).map_err(|_| Error::Unavailable)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .share_mode(SHARE_READ)
            .open(path)
            .map_err(|_| Error::Unavailable)?;
        regular_dir(&file).map_err(|_| Error::Unavailable)?;
        Ok(Self {
            root: Directory(vec![Arc::new(Dir::from_std_file(file))]),
        })
    }

    pub(super) async fn stat(&self, ns: &Namespace, key: &str) -> Result<ObjectInfo, Error> {
        self.read_file(ns, key).await.map(|(info, _)| info)
    }

    pub(super) async fn read(
        &self,
        ns: &Namespace,
        key: &str,
    ) -> Result<(ObjectInfo, Reader), Error> {
        let (info, file) = self.read_file(ns, key).await?;
        Ok((info, Box::new(tokio::fs::File::from_std(file))))
    }

    async fn read_file(&self, ns: &Namespace, key: &str) -> Result<(ObjectInfo, File), Error> {
        let backend = self.clone();
        let ns = ns.clone();
        let key = key.to_owned();
        job(spawn_blocking(move || backend.open_file(&ns, &key))).await
    }

    fn open_file(&self, ns: &Namespace, key: &str) -> Result<(ObjectInfo, File), Error> {
        let data = self.root.path(ns.as_str(), false)?;
        let (dir, name) = data.parent(key, false)?;
        let file = dir.open_regular(name)?;
        let size = file.metadata().map_err(|_| Error::Unavailable)?.len();
        let receipt = self
            .control(ns, false)
            .ok()
            .and_then(|dir| dir.receipt(key));
        Ok((object_info(key, size, None, receipt), file))
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
            let data = match backend.root.path(ns.as_str(), false) {
                Ok(dir) => dir,
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
                match data.path(prefix, false) {
                    Ok(dir) => scan.visit(&dir, prefix)?,
                    Err(Error::NotFound) => scan.key(prefix.to_owned()),
                    Err(err) => return Err(err),
                }
            } else {
                scan.visit(&data, "")?;
            }
            scan.keys
                .into_iter()
                .filter_map(|key| match backend.open_file(&ns, &key) {
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
        let ns = ns.clone();
        let stage_ns = ns.clone();
        let backend = self.clone();
        let (scratch, file) = job(spawn_blocking(move || {
            Scratch::new(backend.control(&stage_ns, true)?)
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
            let _lock = scratch.dir.lock()?;
            let creating = operation == ObjectOperation::Create;
            let result = backend
                .root
                .path(ns.as_str(), creating)
                .and_then(|data| data.parent(&key, creating));
            let (dir, name) = result.map_err(|err| {
                if creating && err == Error::NotFound {
                    Error::Conflict
                } else {
                    err
                }
            })?;
            if creating {
                scratch
                    .dir
                    .current()
                    .hard_link(&scratch.name, dir.current(), name)
                    .map_err(write_error)?;
            } else {
                dir.open_regular(name)?;
                scratch
                    .dir
                    .current()
                    .rename(&scratch.name, dir.current(), name)
                    .map_err(write_error)?;
            }
            file.sync_all().map_err(|_| Error::Uncertain)?;
            let receipt = Receipt::new(operation, &actor, Attribution::default(), None);
            scratch
                .dir
                .write_receipt(&key, &receipt)
                .map_err(|_| Error::Uncertain)?;
            Ok(object_info(&key, size, None, Some(receipt)))
        }))
        .await
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
            let control = backend.control(&ns, true)?;
            let _lock = control.lock()?;
            let data = backend.root.path(ns.as_str(), false)?;
            let (dir, name) = data.parent(&key, false)?;
            dir.open_regular(name)?;
            dir.current().remove_file(name).map_err(write_error)?;
            let receipt = Receipt::new(
                ObjectOperation::Delete,
                &actor,
                Attribution::default(),
                None,
            );
            control
                .write_receipt(&key, &receipt)
                .map_err(|_| Error::Uncertain)?;
            Ok(())
        }))
        .await
    }

    fn control(&self, ns: &Namespace, create: bool) -> Result<Directory, Error> {
        self.root.path(
            &format!("{RECEIPTS}/{RECEIPT_VERSION}/{}", ns.as_str()),
            create,
        )
    }
}

// Keep every ancestor open without write/delete sharing through each path operation.
#[derive(Clone)]
struct Directory(Vec<Arc<Dir>>);

impl Directory {
    fn current(&self) -> &Dir {
        &self.0[self.0.len() - 1]
    }

    fn path(&self, path: &str, create: bool) -> Result<Self, Error> {
        let mut dir = self.clone();
        for component in path.split('/').filter(|part| !part.is_empty()) {
            dir = dir.child(component, create)?;
        }
        Ok(dir)
    }

    fn parent<'a>(&self, key: &'a str, create: bool) -> Result<(Self, &'a str), Error> {
        let (path, name) = key.rsplit_once('/').unwrap_or(("", key));
        Ok((self.path(path, create)?, name))
    }

    fn child(&self, name: &str, create: bool) -> Result<Self, Error> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .follow(FollowSymlinks::No)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .share_mode(SHARE_READ);
        let file = match self.current().open_with(name, &options) {
            Ok(file) => file.into_std(),
            Err(err) if create && err.kind() == io::ErrorKind::NotFound => {
                match self.current().create_dir(name) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(write_error(err)),
                }
                return self.child(name, false);
            }
            Err(err) => return Err(read_error(err)),
        };
        regular_dir(&file)?;
        self.exact(name)?;
        let mut chain = self.0.clone();
        chain.push(Arc::new(Dir::from_std_file(file)));
        Ok(Self(chain))
    }

    fn exact(&self, name: &str) -> Result<(), Error> {
        for (i, entry) in self
            .current()
            .entries()
            .map_err(|_| Error::Unavailable)?
            .enumerate()
        {
            if i >= SCAN_LIMIT {
                return Err(Error::Limit);
            }
            if entry.map_err(|_| Error::Unavailable)?.file_name() == name {
                return Ok(());
            }
        }
        Err(Error::NotFound)
    }

    fn open_regular(&self, name: &str) -> Result<File, Error> {
        self.exact(name)?;
        let file = self
            .current()
            .open_with(
                name,
                OpenOptions::new()
                    .read(true)
                    .follow(FollowSymlinks::No)
                    .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT),
            )
            .map_err(read_error)?
            .into_std();
        regular_file(&file)?;
        Ok(file)
    }

    fn receipt(&self, key: &str) -> Option<Receipt> {
        let file = self.open_regular(&receipt_name(key)).ok()?;
        let mut bytes = Vec::new();
        file.take(RECEIPT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        Receipt::parse(&bytes)
    }

    fn write_receipt(&self, key: &str, receipt: &Receipt) -> Result<(), Error> {
        let (scratch, mut file) = Scratch::new(self.clone())?;
        file.write_all(&receipt.serialize()?)
            .map_err(|_| Error::Unavailable)?;
        file.sync_all().map_err(|_| Error::Unavailable)?;
        self.current()
            .rename(&scratch.name, self.current(), receipt_name(key))
            .map_err(write_error)?;
        file.sync_all().map_err(|_| Error::Unavailable)
    }

    fn lock(&self) -> Result<File, Error> {
        let file = self
            .current()
            .open_with(
                ".write-lock",
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .follow(FollowSymlinks::No)
                    .custom_flags(OPEN_REPARSE_POINT),
            )
            .map_err(write_error)?
            .into_std();
        regular_file(&file)?;
        file.try_lock().map_err(|err| match err {
            TryLockError::WouldBlock => Error::Conflict,
            TryLockError::Error(_) => Error::Unavailable,
        })?;
        Ok(file)
    }
}

fn reparse(metadata: &Metadata) -> bool {
    metadata.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0
}

fn regular_dir(file: &File) -> Result<(), Error> {
    let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
    if metadata.is_dir() && !reparse(&metadata) {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}

fn regular_file(file: &File) -> Result<(), Error> {
    let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
    if metadata.is_file() && !reparse(&metadata) {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}

fn read_error(err: io::Error) -> Error {
    match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => Error::NotFound,
        _ if matches!(err.raw_os_error(), Some(1921 | 681)) => Error::NotFound,
        _ => Error::Unavailable,
    }
}

fn write_error(err: io::Error) -> Error {
    match err.kind() {
        io::ErrorKind::AlreadyExists
        | io::ErrorKind::IsADirectory
        | io::ErrorKind::DirectoryNotEmpty => Error::Conflict,
        io::ErrorKind::PermissionDenied => Error::Conflict,
        _ => read_error(err),
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

struct Scratch {
    dir: Directory,
    name: String,
}

impl Scratch {
    fn new(dir: Directory) -> Result<(Self, File), Error> {
        let name = format!(".pending-{}", Uuid::new_v4());
        let file = dir
            .current()
            .open_with(
                &name,
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .follow(FollowSymlinks::No)
                    .custom_flags(OPEN_REPARSE_POINT),
            )
            .map_err(write_error)?
            .into_std();
        Ok((Self { dir, name }, file))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.dir.current().remove_file(&self.name);
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

    fn visit(&mut self, dir: &Directory, prefix: &str) -> Result<(), Error> {
        for entry in dir.current().entries().map_err(|_| Error::Unavailable)? {
            self.seen += 1;
            if self.seen > SCAN_LIMIT {
                return Err(Error::Limit);
            }
            let entry = entry.map_err(|_| Error::Unavailable)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let key = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if object_key(&key).is_err() {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(Error::Unavailable),
            };
            if metadata.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }
            if metadata.is_file() {
                self.key(key);
            } else if metadata.is_dir() {
                let stem = format!("{key}/");
                if self
                    .after
                    .is_some_and(|after| stem.as_str() < after && !after.starts_with(&stem))
                {
                    continue;
                }
                match dir.child(name, false) {
                    Ok(child) => self.visit(&child, &key)?,
                    Err(Error::NotFound) => {}
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(())
    }
}
