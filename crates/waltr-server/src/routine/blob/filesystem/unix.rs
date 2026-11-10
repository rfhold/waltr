use super::*;
use rustix::fs::{AtFlags, Mode, OFlags, fstat, mkdirat, open, openat, unlinkat};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    os::unix::fs::FileExt,
    path::{Component, Path},
    sync::{
        Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};
use tokio::sync::{Mutex, Notify, mpsc, oneshot, watch};

const FILE_BYTES: u64 = 64 * 1024 * 1024;
const IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const INVOCATION_BYTES: u64 = 512 * 1024 * 1024;
const PRINCIPAL_BYTES: u64 = 1024 * 1024 * 1024;
const PROCESS_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const INVOCATION_OBJECTS: usize = 128;
const PRINCIPAL_OBJECTS: usize = 1024;
const PROCESS_OBJECTS: usize = 4096;
const REQUESTS: usize = 32;
static INSTANCE_ACTIVE: AtomicBool = AtomicBool::new(false);
struct InstanceGuard;
impl InstanceGuard {
    fn acquire() -> Result<Self, BlobError> {
        INSTANCE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| BlobError::Denied)?;
        Ok(Self)
    }
}
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        INSTANCE_ACTIVE.store(false, Ordering::Release);
    }
}

type Completion = (u64, Option<Result<(), BlobError>>);

/// One process-private descriptor-rooted store, with two owned deletion workers.
#[derive(Clone)]
pub struct FilesystemBlobStore {
    client: Arc<Client>,
}
impl std::fmt::Debug for FilesystemBlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemBlobStore")
            .finish_non_exhaustive()
    }
}
struct Client {
    tx: mpsc::Sender<Command>,
    wake: Arc<Notify>,
    shutdown: Arc<AtomicU64>,
    stop: CancellationToken,
    completion: watch::Receiver<Completion>,
    driver: Mutex<Option<tokio::task::JoinHandle<()>>>,
    #[cfg(test)]
    faults: Arc<Faults>,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.stop.cancel();
        self.wake.notify_one();
    }
}

enum Action {
    Open(BlobScope),
    Begin(BlobScope, InvocationOwner, UploadMetadata),
    Append(BlobScope, Upload, u64, Vec<u8>),
    Commit(BlobScope, Upload),
    Reader(BlobScope, CommittedBlob),
    Transfer(BlobScope, CommittedBlob),
    OutputReader(String, OutputLease),
    Read(ReadAccess, BlobReader),
    Close(BlobScope, InvocationOwner),
    Usage,
    Retry,
}
enum Reply {
    Owner(InvocationOwner),
    Upload(Upload),
    Blob(CommittedBlob),
    Reader(BlobReader),
    Lease(OutputLease),
    Chunk(ReadChunk),
    Usage(Usage),
    Unit,
}
struct Command {
    action: Action,
    wait: Wait,
    reply: oneshot::Sender<Result<Reply, BlobError>>,
    receipt: Arc<AtomicU8>,
}
struct CancelOnDrop {
    token: CancellationToken,
    receipt: Arc<AtomicU8>,
    wake: Arc<Notify>,
}
// 0: awaiting delivery, 1: caller received the result, 2: caller abandoned it.
fn receipt_ready(receipt: &Option<Arc<AtomicU8>>) -> Result<(), BlobError> {
    match receipt.as_ref().map(|r| r.load(Ordering::Acquire)) {
        Some(0) => Err(BlobError::Quota),
        Some(2) => Err(BlobError::Stale),
        _ => Ok(()),
    }
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.receipt.load(Ordering::Acquire) == 0 {
            self.receipt.store(2, Ordering::Release);
            self.token.cancel();
            self.wake.notify_one();
        }
    }
}

impl FilesystemBlobStore {
    /// Require a running Tokio runtime, an empty safe root, and no other live
    /// process-instance adapter. Nonblocking exclusive root locking is mandatory.
    pub fn new(config: FilesystemConfig) -> Result<Self, BlobError> {
        tokio::runtime::Handle::try_current().map_err(|_| BlobError::Unavailable)?;
        let instance_guard = InstanceGuard::acquire()?;
        let root = Root::new(&config.private_root, &config.workspace_root)?;
        let (tx, rx) = mpsc::channel(REQUESTS);
        let (done_tx, done_rx) = watch::channel((0, None));
        let wake = Arc::new(Notify::new());
        let shutdown = Arc::new(AtomicU64::new(0));
        let stop = CancellationToken::new();
        #[cfg(test)]
        let faults = Arc::new(Faults::default());
        let state = State {
            instance: Uuid::new_v4(),
            root: Arc::new(root),
            clock: config.clock,
            owners: HashMap::new(),
            objects: HashMap::new(),
            readers: HashMap::new(),
            wake: wake.clone(),
            shutdown: shutdown.clone(),
            stop: stop.clone(),
            closing: false,
            close_waiters: Vec::new(),
            cleanup_waiters: Vec::new(),
            _instance_guard: instance_guard,
            #[cfg(test)]
            faults: faults.clone(),
        };
        let runtime = tokio::runtime::Handle::current();
        let driver =
            tokio::task::spawn_blocking(move || runtime.block_on(state.drive(rx, done_tx)));
        Ok(Self {
            client: Arc::new(Client {
                tx,
                wake,
                shutdown,
                stop,
                completion: done_rx,
                driver: Mutex::new(Some(driver)),
                #[cfg(test)]
                faults,
            }),
        })
    }

    fn call<T: Send + 'static>(
        &self,
        action: Action,
        wait: Wait,
        extract: fn(Reply) -> Result<T, BlobError>,
    ) -> BlobFuture<T> {
        let valid = match &action {
            Action::Open(scope)
            | Action::Begin(scope, ..)
            | Action::Append(scope, ..)
            | Action::Commit(scope, ..)
            | Action::Reader(scope, ..)
            | Action::Transfer(scope, ..)
            | Action::Close(scope, ..)
            | Action::Read(ReadAccess::Invocation(scope), ..) => valid_scope(scope),
            Action::OutputReader(principal, ..)
            | Action::Read(ReadAccess::Output { principal }, ..) => {
                !principal.is_empty()
                    && principal.len() <= 256
                    && !principal.chars().any(char::is_control)
            }
            Action::Usage | Action::Retry => true,
        } && match &action {
            Action::Begin(_, _, metadata) => metadata_valid(metadata),
            _ => true,
        };
        if !valid {
            return Box::pin(async { Err(BlobError::Invalid) });
        }
        if let Err(error) = wait.check() {
            return Box::pin(async move { Err(error) });
        }
        if self.client.shutdown.load(Ordering::Acquire) != 0 {
            return Box::pin(async { Err(BlobError::Stale) });
        }
        let token = wait.cancellation.child_token();
        let receipt = Arc::new(AtomicU8::new(0));
        let guard = CancelOnDrop {
            token: token.clone(),
            receipt: receipt.clone(),
            wake: self.client.wake.clone(),
        };
        let wait = Wait {
            cancellation: token,
            ..wait
        };
        let (reply, mut receive) = oneshot::channel();
        let command = Command {
            action,
            wait: wait.clone(),
            reply,
            receipt,
        };
        if self.client.tx.try_send(command).is_err() {
            return Box::pin(async { Err(BlobError::Quota) });
        }
        Box::pin(async move {
            let guard = guard;
            tokio::select! {
                biased;
                () = wait.cancellation.cancelled() => Err(BlobError::Cancelled),
                () = tokio::time::sleep_until(wait.deadline) => Err(BlobError::Deadline),
                result = &mut receive => {
                    let result = extract(result.map_err(|_| BlobError::Unavailable)??);
                    guard.receipt.store(1, Ordering::Release);
                    result
                },
            }
        })
    }
}

macro_rules! extract {
    ($variant:ident) => {
        |reply| match reply {
            Reply::$variant(value) => Ok(value),
            _ => Err(BlobError::Unavailable),
        }
    };
}
fn unit(reply: Reply) -> Result<(), BlobError> {
    if matches!(reply, Reply::Unit) {
        Ok(())
    } else {
        Err(BlobError::Unavailable)
    }
}
impl BlobStore for FilesystemBlobStore {
    fn open_invocation(&self, scope: BlobScope, wait: Wait) -> BlobFuture<InvocationOwner> {
        self.call(Action::Open(scope), wait, extract!(Owner))
    }
    fn begin(
        &self,
        scope: BlobScope,
        owner: InvocationOwner,
        metadata: UploadMetadata,
        wait: Wait,
    ) -> BlobFuture<Upload> {
        self.call(
            Action::Begin(scope, owner, metadata),
            wait,
            extract!(Upload),
        )
    }
    fn append(
        &self,
        scope: BlobScope,
        upload: Upload,
        sequence: u64,
        bytes: Vec<u8>,
        wait: Wait,
    ) -> BlobFuture<()> {
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
            return Box::pin(async { Err(BlobError::Invalid) });
        }
        self.call(Action::Append(scope, upload, sequence, bytes), wait, unit)
    }
    fn commit(&self, scope: BlobScope, upload: Upload, wait: Wait) -> BlobFuture<CommittedBlob> {
        self.call(Action::Commit(scope, upload), wait, extract!(Blob))
    }
    fn reader(&self, scope: BlobScope, blob: CommittedBlob, wait: Wait) -> BlobFuture<BlobReader> {
        self.call(Action::Reader(scope, blob), wait, extract!(Reader))
    }
    fn transfer_output(
        &self,
        scope: BlobScope,
        blob: CommittedBlob,
        wait: Wait,
    ) -> BlobFuture<OutputLease> {
        self.call(Action::Transfer(scope, blob), wait, extract!(Lease))
    }
    fn output_reader(
        &self,
        principal: String,
        lease: OutputLease,
        wait: Wait,
    ) -> BlobFuture<BlobReader> {
        self.call(
            Action::OutputReader(principal, lease),
            wait,
            extract!(Reader),
        )
    }
    fn read(&self, access: ReadAccess, reader: BlobReader, wait: Wait) -> BlobFuture<ReadChunk> {
        self.call(Action::Read(access, reader), wait, extract!(Chunk))
    }
    fn close_invocation(
        &self,
        scope: BlobScope,
        owner: InvocationOwner,
        wait: Wait,
    ) -> BlobFuture<()> {
        if owner.scope != scope {
            return Box::pin(async { Err(BlobError::Denied) });
        }
        owner.signal.revoke();
        self.call(Action::Close(scope, owner), wait, unit)
    }
    fn usage(&self, wait: Wait) -> BlobFuture<Usage> {
        self.call(Action::Usage, wait, extract!(Usage))
    }
    fn retry_cleanup(&self, wait: Wait) -> BlobFuture<()> {
        self.call(Action::Retry, wait, unit)
    }
    fn shutdown(&self) -> BlobFuture<()> {
        let client = self.client.clone();
        let epoch = client
            .shutdown
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        client.stop.cancel();
        client.wake.notify_one();
        Box::pin(async move {
            let mut completion = client.completion.clone();
            loop {
                let (generation, result) = *completion.borrow_and_update();
                if (generation >= epoch || result == Some(Ok(())))
                    && let Some(result) = result
                {
                    result?;
                    let mut driver = client.driver.lock().await;
                    if let Some(handle) = driver.as_mut() {
                        handle.await.map_err(|_| BlobError::Cleanup)?;
                        driver.take();
                    }
                    return Ok(());
                }
                completion.changed().await.map_err(|_| BlobError::Cleanup)?;
            }
        })
    }
}

struct Root {
    parent: File,
    directory: File,
    name: String,
}
fn io<T>(result: rustix::io::Result<T>) -> Result<T, BlobError> {
    result.map_err(|_| BlobError::Unavailable)
}
fn directories(path: &Path) -> Result<Vec<File>, BlobError> {
    if !path.is_absolute() {
        return Err(BlobError::Invalid);
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut result = vec![File::from(io(open("/", flags, Mode::empty()))?)];
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                result.push(File::from(io(openat(
                    result.last().ok_or(BlobError::Invalid)?,
                    name,
                    flags,
                    Mode::empty(),
                ))?));
            }
            _ => return Err(BlobError::Invalid),
        }
    }
    Ok(result)
}
fn same(a: &File, b: &File) -> Result<bool, BlobError> {
    let a = io(fstat(a))?;
    let b = io(fstat(b))?;
    Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
}
impl Root {
    fn new(path: &Path, workspace: &WorkspaceRoot) -> Result<Self, BlobError> {
        let mut chain = directories(path)?;
        let root = chain.last().ok_or(BlobError::Invalid)?;
        match workspace {
            WorkspaceRoot::Local(workspace) => {
                let workspace = directories(workspace)?;
                let worker = workspace.last().ok_or(BlobError::Invalid)?;
                for ancestor in &chain {
                    if same(ancestor, worker)? {
                        return Err(BlobError::Denied);
                    }
                }
                for ancestor in &workspace {
                    if same(ancestor, root)? {
                        return Err(BlobError::Denied);
                    }
                }
            }
            WorkspaceRoot::SocketWorker(workspace) => {
                if !workspace.is_absolute()
                    || workspace
                        .components()
                        .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
                {
                    return Err(BlobError::Invalid);
                }
                if crate::workspace_paths::overlaps(workspace, path)
                    .map_err(|_| BlobError::Unavailable)?
                {
                    return Err(BlobError::Denied);
                }
            }
        }
        let uid = rustix::process::geteuid().as_raw();
        for ancestor in &chain {
            let stat = io(fstat(ancestor))?;
            if stat.st_uid != uid && stat.st_uid != 0 {
                return Err(BlobError::Denied);
            }
            if stat.st_mode & 0o022 != 0 {
                return Err(BlobError::Denied);
            }
        }
        let stat = io(fstat(root))?;
        if stat.st_uid != uid || stat.st_mode & 0o777 != 0o700 {
            return Err(BlobError::Denied);
        }
        let parent = chain.pop().ok_or(BlobError::Invalid)?;
        rustix::fs::flock(
            &parent,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .map_err(|_| BlobError::Denied)?;
        let entries = io(rustix::fs::Dir::read_from(&parent))?;
        for entry in entries {
            let entry = io(entry)?;
            if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
                return Err(BlobError::Cleanup);
            }
        }
        let name = format!("blob-{}", Uuid::new_v4());
        io(mkdirat(&parent, name.as_str(), Mode::from_raw_mode(0o700)))?;
        let directory = match openat(
            &parent,
            name.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(_) => {
                let _ = unlinkat(&parent, name.as_str(), AtFlags::REMOVEDIR);
                return Err(BlobError::Unavailable);
            }
        };
        let safe =
            fstat(&directory).is_ok_and(|stat| stat.st_uid == uid && stat.st_mode & 0o777 == 0o700);
        if !safe {
            drop(directory);
            let _removed = unlinkat(&parent, name.as_str(), AtFlags::REMOVEDIR);
            return Err(BlobError::Denied);
        }
        Ok(Self {
            parent,
            directory,
            name,
        })
    }
    fn create(&self, name: &str) -> Result<File, BlobError> {
        let file = File::from(io(openat(
            &self.directory,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ))?);
        Ok(file)
    }
    fn delete(&self, name: &str) -> Result<(), BlobError> {
        match unlinkat(&self.directory, name, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(_) => Err(BlobError::Cleanup),
        }
    }
    fn remove_directory(&self) -> Result<(), BlobError> {
        let current = io(rustix::fs::statat(
            &self.parent,
            self.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        ))?;
        let expected = io(fstat(&self.directory))?;
        if current.st_dev != expected.st_dev || current.st_ino != expected.st_ino {
            return Err(BlobError::Cleanup);
        }
        unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR)
            .map_err(|_| BlobError::Cleanup)
    }
}

struct OwnerEntry {
    scope: BlobScope,
    signal: Weak<Signal>,
    closed: bool,
}
enum ObjectState {
    Upload {
        sequence: u64,
        signal: Weak<Signal>,
        hash: Sha256,
    },
    Committed(BlobMetadata),
    Output {
        lease: Identity,
        expiry: u64,
    },
    Deleting {
        in_flight: bool,
        failed: bool,
    },
}
struct Object {
    scope: BlobScope,
    owner: Identity,
    name: String,
    file: Option<Arc<File>>,
    metadata: UploadMetadata,
    bytes: u64,
    charged: u64,
    state: ObjectState,
    receipt: Option<Arc<AtomicU8>>,
}
struct ReaderEntry {
    object: Identity,
    access: ReadAccess,
    offset: u64,
    signal: Weak<Signal>,
    receipt: Option<Arc<AtomicU8>>,
}
struct CloseWaiter {
    owner: Identity,
    reply: oneshot::Sender<Result<Reply, BlobError>>,
    wait: Wait,
}
struct State {
    instance: Uuid,
    root: Arc<Root>,
    clock: Arc<dyn Clock>,
    owners: HashMap<Identity, OwnerEntry>,
    objects: HashMap<Identity, Object>,
    readers: HashMap<Identity, ReaderEntry>,
    wake: Arc<Notify>,
    shutdown: Arc<AtomicU64>,
    stop: CancellationToken,
    closing: bool,
    close_waiters: Vec<CloseWaiter>,
    cleanup_waiters: Vec<CleanupWaiter>,
    _instance_guard: InstanceGuard,
    #[cfg(test)]
    faults: Arc<Faults>,
}
struct Delete {
    id: Identity,
    name: String,
    file: Option<Arc<File>>,
}
type CleanupWaiter = (Wait, oneshot::Sender<Result<Reply, BlobError>>);

fn live(signal: &Weak<Signal>) -> bool {
    signal
        .upgrade()
        .is_some_and(|signal| !signal.revoked.load(Ordering::Acquire))
}
fn valid_scope(scope: &BlobScope) -> bool {
    [&scope.principal, &scope.invocation]
        .iter()
        .all(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
}
fn metadata_valid(metadata: &UploadMetadata) -> bool {
    let name = &metadata.display_name;
    let mime = &metadata.content_type;
    let mime_matches = match metadata.kind {
        BlobKind::File => true,
        BlobKind::Image(ImageFormat::Png) => mime == "image/png",
        BlobKind::Image(ImageFormat::Jpeg) => mime == "image/jpeg",
        BlobKind::Image(ImageFormat::WebP) => mime == "image/webp",
    };
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        && !mime.is_empty()
        && mime.len() <= 128
        && mime.is_ascii()
        && !mime
            .bytes()
            .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
        && mime.contains('/')
        && mime_matches
        && metadata
            .declared_bytes
            .is_none_or(|bytes| bytes <= object_limit(metadata.kind))
}
fn object_limit(kind: BlobKind) -> u64 {
    match kind {
        BlobKind::File => FILE_BYTES,
        BlobKind::Image(_) => IMAGE_BYTES,
    }
}

impl State {
    fn identity(&self) -> Identity {
        Identity {
            instance: self.instance,
            token: Uuid::new_v4(),
        }
    }
    fn signal(&self) -> Arc<Signal> {
        Arc::new(Signal {
            revoked: AtomicBool::new(false),
            wake: self.wake.clone(),
        })
    }
    fn owner(&self, scope: &BlobScope, id: Identity) -> Result<&OwnerEntry, BlobError> {
        let owner = self.owners.get(&id).ok_or(BlobError::Stale)?;
        if owner.scope != *scope {
            return Err(BlobError::Denied);
        }
        if owner.closed || !live(&owner.signal) {
            return Err(BlobError::Stale);
        }
        Ok(owner)
    }
    fn object(&self, scope: &BlobScope, id: Identity) -> Result<&Object, BlobError> {
        let object = self.objects.get(&id).ok_or(BlobError::Stale)?;
        if object.scope != *scope {
            return Err(BlobError::Denied);
        }
        self.owner(scope, object.owner)?;
        receipt_ready(&object.receipt)?;
        Ok(object)
    }
    fn reserve(&self, scope: &BlobScope, extra: u64, new: bool) -> Result<(), BlobError> {
        #[cfg(not(test))]
        let (invocation_limit, principal_limit, process_limit) =
            (INVOCATION_BYTES, PRINCIPAL_BYTES, PROCESS_BYTES);
        #[cfg(test)]
        let (invocation_limit, principal_limit, process_limit) = self
            .faults
            .byte_limits
            .lock()
            .unwrap()
            .unwrap_or((INVOCATION_BYTES, PRINCIPAL_BYTES, PROCESS_BYTES));
        let mut process = 0u64;
        let mut principal = 0u64;
        let mut invocation = 0u64;
        let mut principals = 0;
        let mut invocations = 0;
        for object in self.objects.values() {
            process += object.charged;
            if object.scope.principal == scope.principal {
                principal += object.charged;
                principals += 1;
            }
            if object.scope == *scope {
                invocation += object.charged;
                invocations += 1;
            }
        }
        if process.saturating_add(extra) > process_limit
            || principal.saturating_add(extra) > principal_limit
            || invocation.saturating_add(extra) > invocation_limit
            || (new
                && (self.objects.len() >= PROCESS_OBJECTS
                    || principals >= PRINCIPAL_OBJECTS
                    || invocations >= INVOCATION_OBJECTS))
        {
            return Err(BlobError::Quota);
        }
        Ok(())
    }
    fn revoke(&mut self, id: Identity) {
        if let Some(object) = self.objects.get_mut(&id)
            && !matches!(object.state, ObjectState::Deleting { .. })
        {
            object.state = ObjectState::Deleting {
                in_flight: false,
                failed: false,
            };
        }
        self.readers.retain(|_, reader| reader.object != id);
    }
    fn sweep(&mut self) {
        for owner in self.owners.values_mut() {
            if !live(&owner.signal) {
                owner.closed = true;
            }
        }
        let now = self.clock.now_ms();
        let revoke: Vec<_> = self
            .objects
            .iter()
            .filter_map(|(id, object)| {
                let expired = match &object.state {
                    ObjectState::Output { expiry, .. } => now >= *expiry || self.closing,
                    ObjectState::Deleting { .. } => false,
                    ObjectState::Upload { signal, .. } => {
                        !live(signal)
                            || self.owners.get(&object.owner).is_none_or(|o| o.closed)
                            || self.closing
                    }
                    ObjectState::Committed(_) => {
                        self.owners.get(&object.owner).is_none_or(|o| o.closed) || self.closing
                    }
                };
                (expired
                    || object
                        .receipt
                        .as_ref()
                        .is_some_and(|r| r.load(Ordering::Acquire) == 2))
                .then_some(*id)
            })
            .collect();
        for id in revoke {
            self.revoke(id);
        }
        self.readers.retain(|_, reader| {
            live(&reader.signal)
                && reader
                    .receipt
                    .as_ref()
                    .is_none_or(|r| r.load(Ordering::Acquire) != 2)
        });
        self.owners.retain(|id, owner| {
            !owner.closed
                || owner.signal.strong_count() != 0
                || self.objects.values().any(|object| object.owner == *id)
                || self.close_waiters.iter().any(|waiter| waiter.owner == *id)
        });
    }
    fn retry(&mut self) {
        for object in self.objects.values_mut() {
            if let ObjectState::Deleting {
                in_flight: false,
                failed,
            } = &mut object.state
            {
                *failed = false;
            }
        }
    }
    fn usage(&self) -> Usage {
        Usage {
            bytes: self.objects.values().map(|o| o.charged).sum(),
            objects: self.objects.len(),
            readers: self.readers.len(),
            pending_deletions: self
                .objects
                .values()
                .filter(|o| matches!(o.state, ObjectState::Deleting { .. }))
                .count(),
            deleting: self
                .objects
                .values()
                .filter(|o| {
                    matches!(
                        o.state,
                        ObjectState::Deleting {
                            in_flight: true,
                            ..
                        }
                    )
                })
                .count(),
        }
    }
    fn new_reader(
        &mut self,
        object: Identity,
        access: ReadAccess,
    ) -> Result<BlobReader, BlobError> {
        let entry = self.objects.get(&object).ok_or(BlobError::Stale)?;
        if self.readers.values().filter(|r| r.object == object).count() >= 4
            || self
                .readers
                .values()
                .filter(|r| {
                    self.objects
                        .get(&r.object)
                        .is_some_and(|o| o.scope == entry.scope)
                })
                .count()
                >= 32
        {
            return Err(BlobError::Quota);
        }
        let id = self.identity();
        let signal = self.signal();
        self.readers.insert(
            id,
            ReaderEntry {
                object,
                access,
                offset: 0,
                signal: Arc::downgrade(&signal),
                receipt: None,
            },
        );
        Ok(BlobReader { id, signal })
    }
    async fn action(&mut self, action: Action, wait: &Wait) -> Result<Reply, BlobError> {
        wait.check()?;
        match action {
            Action::Open(scope) => {
                if !valid_scope(&scope) {
                    return Err(BlobError::Invalid);
                }
                if self.owners.len() >= PROCESS_OBJECTS {
                    return Err(BlobError::Quota);
                }
                if self.owners.values().any(|o| o.scope == scope)
                    || self.objects.values().any(|o| o.scope == scope)
                {
                    return Err(BlobError::Denied);
                }
                let id = self.identity();
                let signal = self.signal();
                self.owners.insert(
                    id,
                    OwnerEntry {
                        scope: scope.clone(),
                        signal: Arc::downgrade(&signal),
                        closed: false,
                    },
                );
                Ok(Reply::Owner(InvocationOwner { id, scope, signal }))
            }
            Action::Begin(scope, owner, metadata) => {
                self.owner(&scope, owner.id)?;
                if !metadata_valid(&metadata) {
                    return Err(BlobError::Invalid);
                }
                let charged = metadata.declared_bytes.unwrap_or(0);
                self.reserve(&scope, charged, true)?;
                let id = self.identity();
                let signal = self.signal();
                let name = Uuid::new_v4().to_string();
                let file = self.root.create(&name)?;
                self.objects.insert(
                    id,
                    Object {
                        scope,
                        owner: owner.id,
                        name,
                        file: Some(Arc::new(file)),
                        metadata,
                        bytes: 0,
                        charged,
                        state: ObjectState::Upload {
                            sequence: 0,
                            signal: Arc::downgrade(&signal),
                            hash: Sha256::new(),
                        },
                        receipt: None,
                    },
                );
                let file = self.objects[&id]
                    .file
                    .as_ref()
                    .ok_or(BlobError::Unavailable)?;
                let safe = rustix::fs::fchmod(file, Mode::from_raw_mode(0o600))
                    .and_then(|()| fstat(file))
                    .is_ok_and(|stat| {
                        stat.st_uid == rustix::process::geteuid().as_raw()
                            && stat.st_mode & 0o777 == 0o600
                            && stat.st_nlink == 1
                    });
                if !safe {
                    self.revoke(id);
                    return Err(BlobError::Denied);
                }
                Ok(Reply::Upload(Upload { id, signal }))
            }
            Action::Append(scope, upload, sequence, bytes) => {
                let object = self.object(&scope, upload.id)?;
                if upload.signal.revoked.load(Ordering::Acquire) {
                    return Err(BlobError::Stale);
                }
                let ObjectState::Upload {
                    sequence: expected,
                    signal,
                    ..
                } = &object.state
                else {
                    return Err(BlobError::Stale);
                };
                if !live(signal) {
                    return Err(BlobError::Stale);
                }
                if sequence != *expected {
                    self.revoke(upload.id);
                    return Err(BlobError::Sequence);
                }
                let total = object
                    .bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or(BlobError::Quota)?;
                if total > object_limit(object.metadata.kind)
                    || object.metadata.declared_bytes.is_some_and(|n| total > n)
                {
                    self.revoke(upload.id);
                    return Err(BlobError::Quota);
                }
                let extra = total.saturating_sub(object.charged);
                if let Err(error) = self.reserve(&scope, extra, false) {
                    self.revoke(upload.id);
                    return Err(error);
                }
                let object = self.objects.get_mut(&upload.id).ok_or(BlobError::Stale)?;
                object.charged += extra;
                let file = object.file.as_ref().ok_or(BlobError::Unavailable)?;
                #[cfg(test)]
                if self.faults.partial_write.swap(false, Ordering::AcqRel) {
                    drop(file.write_at(&bytes[..bytes.len() / 2], object.bytes));
                    self.revoke(upload.id);
                    return Err(BlobError::Unavailable);
                }
                if let Err(error) = write_all(file, &bytes, object.bytes, wait) {
                    self.revoke(upload.id);
                    return Err(error);
                }
                object.bytes = total;
                if let ObjectState::Upload { sequence, hash, .. } = &mut object.state {
                    *sequence = sequence.checked_add(1).ok_or(BlobError::Quota)?;
                    hash.update(&bytes);
                }
                if let Err(error) = wait.check() {
                    self.revoke(upload.id);
                    return Err(error);
                }
                Ok(Reply::Unit)
            }
            Action::Commit(scope, upload) => {
                if !matches!(
                    self.object(&scope, upload.id)?.state,
                    ObjectState::Upload { .. }
                ) {
                    return Err(BlobError::Stale);
                }
                let result = self.commit(&scope, &upload, wait).await;
                if result.is_err() {
                    self.revoke(upload.id);
                }
                result.map(Reply::Blob)
            }
            Action::Reader(scope, blob) => {
                if !matches!(
                    self.object(&scope, blob.id)?.state,
                    ObjectState::Committed(_)
                ) {
                    return Err(BlobError::Stale);
                }
                self.new_reader(blob.id, ReadAccess::Invocation(scope))
                    .map(Reply::Reader)
            }
            Action::Transfer(scope, blob) => {
                let ObjectState::Committed(metadata) = &self.object(&scope, blob.id)?.state else {
                    return Err(BlobError::Stale);
                };
                let metadata = metadata.clone();
                let expiry = self
                    .clock
                    .now_ms()
                    .checked_add(OUTPUT_LEASE_MS)
                    .ok_or(BlobError::Unavailable)?;
                let lease = self.identity();
                self.readers.retain(|_, reader| reader.object != blob.id);
                self.objects
                    .get_mut(&blob.id)
                    .ok_or(BlobError::Stale)?
                    .state = ObjectState::Output { lease, expiry };
                Ok(Reply::Lease(OutputLease {
                    id: lease,
                    metadata,
                    expires_at_ms: expiry,
                }))
            }
            Action::OutputReader(principal, lease) => {
                let object = self.objects.iter().find(|(_, o)| matches!(&o.state, ObjectState::Output { lease: id, .. } if *id == lease.id)).map(|(id, _)| *id).ok_or(BlobError::Stale)?;
                if self.objects[&object].scope.principal != principal {
                    return Err(BlobError::Denied);
                }
                self.new_reader(object, ReadAccess::Output { principal })
                    .map(Reply::Reader)
            }
            Action::Read(access, reader) => self.read(access, reader, wait).map(Reply::Chunk),
            Action::Usage => Ok(Reply::Usage(self.usage())),
            Action::Close(..) | Action::Retry => Err(BlobError::Invalid),
        }
    }
    async fn commit(
        &mut self,
        scope: &BlobScope,
        upload: &Upload,
        wait: &Wait,
    ) -> Result<CommittedBlob, BlobError> {
        let object = self.object(scope, upload.id)?;
        let ObjectState::Upload { hash, signal, .. } = &object.state else {
            return Err(BlobError::Stale);
        };
        if !live(signal)
            || object
                .metadata
                .declared_bytes
                .is_some_and(|n| n != object.bytes)
        {
            return Err(BlobError::Invalid);
        }
        let digest: [u8; 32] = hash.clone().finalize().into();
        if object.metadata.expected_sha256.is_some_and(|d| d != digest) {
            return Err(BlobError::Invalid);
        }
        let image = if let BlobKind::Image(format) = object.metadata.kind {
            let signature = read_chunk(
                object.file.as_ref().ok_or(BlobError::Unavailable)?,
                0,
                12,
                object.bytes,
                wait,
                #[cfg(test)]
                Some(&self.faults),
            )?;
            let matches = match format {
                ImageFormat::Png => signature.starts_with(b"\x89PNG\r\n\x1a\n"),
                ImageFormat::Jpeg => signature.starts_with(b"\xff\xd8"),
                ImageFormat::WebP => signature.len() >= 12
                    && &signature[..4] == b"RIFF"
                    && &signature[8..12] == b"WEBP",
            };
            if !matches {
                return Err(BlobError::Invalid);
            }
            Some(format)
        } else {
            None
        };
        wait.check()?;
        if self.stop.is_cancelled() {
            return Err(BlobError::Cancelled);
        }
        self.owner(scope, object.owner)?;
        let metadata = BlobMetadata {
            display_name: object.metadata.display_name.clone(),
            content_type: object.metadata.content_type.clone(),
            bytes: object.bytes,
            sha256: digest,
            image,
        };
        self.objects
            .get_mut(&upload.id)
            .ok_or(BlobError::Stale)?
            .state = ObjectState::Committed(metadata.clone());
        Ok(CommittedBlob {
            id: upload.id,
            metadata,
        })
    }
    fn read(
        &mut self,
        access: ReadAccess,
        reader: BlobReader,
        wait: &Wait,
    ) -> Result<ReadChunk, BlobError> {
        let entry = self.readers.get(&reader.id).ok_or(BlobError::Stale)?;
        if !live(&entry.signal) {
            return Err(BlobError::Stale);
        }
        receipt_ready(&entry.receipt)?;
        let allowed = match (&entry.access, &access) {
            (ReadAccess::Invocation(a), ReadAccess::Invocation(b)) => a == b,
            (ReadAccess::Output { principal: a }, ReadAccess::Output { principal: b }) => a == b,
            _ => false,
        };
        if !allowed {
            return Err(BlobError::Denied);
        }
        let object = self.objects.get(&entry.object).ok_or(BlobError::Stale)?;
        match &object.state {
            ObjectState::Committed(_) => {
                self.owner(&object.scope, object.owner)?;
            }
            ObjectState::Output { expiry, .. } if self.clock.now_ms() < *expiry => {}
            _ => return Err(BlobError::Stale),
        }
        let bytes = read_chunk(
            object.file.as_ref().ok_or(BlobError::Unavailable)?,
            entry.offset,
            CHUNK_BYTES,
            object.bytes,
            wait,
            #[cfg(test)]
            Some(&self.faults),
        )?;
        if !live(&entry.signal) {
            return Err(BlobError::Stale);
        }
        match &object.state {
            ObjectState::Output { expiry, .. } if self.clock.now_ms() >= *expiry => {
                return Err(BlobError::Stale);
            }
            ObjectState::Committed(_) => {
                self.owner(&object.scope, object.owner)?;
            }
            _ => {}
        }
        let offset = entry.offset + bytes.len() as u64;
        self.readers
            .get_mut(&reader.id)
            .ok_or(BlobError::Stale)?
            .offset = offset;
        Ok(ReadChunk {
            bytes,
            eof: offset == object.bytes,
        })
    }
}

fn write_all(file: &File, bytes: &[u8], offset: u64, wait: &Wait) -> Result<(), BlobError> {
    let mut written = 0;
    while written < bytes.len() {
        wait.check()?;
        let count = file
            .write_at(&bytes[written..], offset + written as u64)
            .map_err(|_| BlobError::Unavailable)?;
        if count == 0 {
            return Err(BlobError::Unavailable);
        }
        written += count;
    }
    Ok(())
}
fn read_chunk(
    file: &File,
    offset: u64,
    maximum: usize,
    len: u64,
    wait: &Wait,
    #[cfg(test)] faults: Option<&Faults>,
) -> Result<Vec<u8>, BlobError> {
    wait.check()?;
    if maximum == 0 || maximum > CHUNK_BYTES || offset > len {
        return Err(BlobError::Invalid);
    }
    let mut bytes = vec![0; maximum.min((len - offset) as usize)];
    let mut read = 0;
    while read < bytes.len() {
        wait.check()?;
        let count = file
            .read_at(&mut bytes[read..], offset + read as u64)
            .map_err(|_| BlobError::Unavailable)?;
        #[cfg(test)]
        if let Some(faults) = faults {
            faults.read_bytes.fetch_add(count as u64, Ordering::AcqRel);
            let lock = faults.gate_lock.lock().unwrap();
            if faults.read_gate.load(Ordering::Acquire) {
                faults.read_entered.store(true, Ordering::Release);
                let (_lock, timeout) = faults
                    .gate_wake
                    .wait_timeout_while(lock, std::time::Duration::from_secs(5), |_| {
                        faults.read_gate.load(Ordering::Acquire)
                    })
                    .unwrap();
                if timeout.timed_out() {
                    return Err(BlobError::Unavailable);
                }
            }
        }
        if count == 0 {
            return Err(BlobError::Invalid);
        }
        read += count;
    }
    wait.check()?;
    Ok(bytes)
}

#[cfg(test)]
#[derive(Default)]
pub(in crate::routine::blob) struct Faults {
    pub byte_limits: std::sync::Mutex<Option<(u64, u64, u64)>>,
    pub read_gate: AtomicBool,
    pub read_entered: AtomicBool,
    pub read_bytes: AtomicU64,
    pub deletes_attempted: std::sync::atomic::AtomicUsize,
    pub delete_failures: std::sync::atomic::AtomicUsize,
    pub partial_write: AtomicBool,
    pub deletes_active: std::sync::atomic::AtomicUsize,
    pub deletes_peak: std::sync::atomic::AtomicUsize,
    pub delete_gate: AtomicBool,
    pub gate_wake: std::sync::Condvar,
    pub gate_lock: std::sync::Mutex<()>,
}

impl State {
    async fn drive(
        mut self,
        mut rx: mpsc::Receiver<Command>,
        completion: watch::Sender<Completion>,
    ) {
        let (jobs, work) = mpsc::channel::<Delete>(PROCESS_OBJECTS);
        let work = Arc::new(Mutex::new(work));
        let (results, mut deleted) = mpsc::channel(2);
        let mut workers = Vec::new();
        for _ in 0..2 {
            let work = work.clone();
            let results = results.clone();
            let root = self.root.clone();
            #[cfg(test)]
            let faults = self.faults.clone();
            workers.push(tokio::spawn(async move {
                loop {
                    let Some(job) = work.lock().await.recv().await else {
                        break;
                    };
                    let root = root.clone();
                    #[cfg(test)]
                    let faults = faults.clone();
                    let id = job.id;
                    let result = tokio::task::spawn_blocking(move || {
                        #[cfg(test)]
                        let gate_timed_out = {
                            let active = faults.deletes_active.fetch_add(1, Ordering::AcqRel) + 1;
                            faults.deletes_attempted.fetch_add(1, Ordering::AcqRel);
                            faults.deletes_peak.fetch_max(active, Ordering::AcqRel);
                            let lock = faults.gate_lock.lock().unwrap();
                            let (_lock, timeout) = faults
                                .gate_wake
                                .wait_timeout_while(lock, std::time::Duration::from_secs(5), |_| {
                                    faults.delete_gate.load(Ordering::Acquire)
                                })
                                .unwrap();
                            timeout.timed_out()
                        };
                        drop(job.file);
                        #[cfg(test)]
                        let result = if gate_timed_out
                            || faults
                                .delete_failures
                                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                    n.checked_sub(1)
                                })
                                .is_ok()
                        {
                            Err(BlobError::Cleanup)
                        } else {
                            root.delete(&job.name)
                        };
                        #[cfg(not(test))]
                        let result = root.delete(&job.name);
                        #[cfg(test)]
                        faults.deletes_active.fetch_sub(1, Ordering::AcqRel);
                        result
                    })
                    .await
                    .unwrap_or(Err(BlobError::Cleanup));
                    if results.send((id, result)).await.is_err() {
                        break;
                    }
                }
            }));
        }
        drop(results);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut attempted = 0;
        let mut disconnected = false;
        loop {
            self.sweep();
            let requested = self.shutdown.load(Ordering::Acquire);
            if requested > attempted || (disconnected && !self.closing) {
                self.closing = true;
                attempted = requested.max(1);
                self.retry();
                for owner in self.owners.values_mut() {
                    owner.closed = true;
                }
                self.sweep();
            }
            for (id, object) in &mut self.objects {
                if let ObjectState::Deleting {
                    in_flight,
                    failed: false,
                } = &mut object.state
                    && !*in_flight
                {
                    let job = Delete {
                        id: *id,
                        name: object.name.clone(),
                        file: object.file.take(),
                    };
                    match jobs.try_send(job) {
                        Ok(()) => *in_flight = true,
                        Err(error) => {
                            object.file = error.into_inner().file;
                        }
                    }
                }
            }
            self.finish_waiters();
            let pending = self.objects.values().any(|o| {
                matches!(
                    o.state,
                    ObjectState::Deleting {
                        in_flight: true,
                        ..
                    } | ObjectState::Deleting { failed: false, .. }
                )
            });
            if self.closing && !pending {
                if self.objects.is_empty() {
                    drop(jobs);
                    let mut result = self.root.remove_directory();
                    for worker in workers {
                        if worker.await.is_err() {
                            result = Err(BlobError::Cleanup);
                        }
                    }
                    let _ = completion.send((
                        self.shutdown.load(Ordering::Acquire).max(attempted),
                        Some(result),
                    ));
                    return;
                }
                let _ = completion.send((
                    self.shutdown.load(Ordering::Acquire).max(attempted),
                    Some(Err(BlobError::Cleanup)),
                ));
            }
            tokio::select! {
                biased;
                Some((id, result)) = deleted.recv() => {
                    if result.is_ok() { self.objects.remove(&id); }
                    else if let Some(object) = self.objects.get_mut(&id) { object.state = ObjectState::Deleting { in_flight: false, failed: true }; }
                }
                () = self.wake.notified() => {}
                _ = tick.tick() => { if disconnected { self.retry(); } }
                command = rx.recv(), if !disconnected => {
                    match command {
                        None => disconnected = true,
                        Some(command) => self.command(command).await,
                    }
                }
            }
        }
    }
    async fn command(&mut self, command: Command) {
        let Command {
            action,
            wait,
            reply,
            receipt,
        } = command;
        if self.closing {
            drop(reply.send(Err(BlobError::Stale)));
            return;
        }
        if let Err(error) = wait.check() {
            drop(reply.send(Err(error)));
            return;
        }
        match action {
            Action::Close(scope, owner) => {
                let result = self
                    .owners
                    .get_mut(&owner.id)
                    .ok_or(BlobError::Stale)
                    .and_then(|entry| {
                        if entry.scope != scope {
                            return Err(BlobError::Denied);
                        }
                        entry.closed = true;
                        owner.signal.revoke();
                        Ok(())
                    });
                if let Err(error) = result {
                    drop(reply.send(Err(error)));
                } else if self.close_waiters.len() >= REQUESTS {
                    drop(reply.send(Err(BlobError::Quota)));
                } else {
                    self.retry();
                    self.close_waiters.push(CloseWaiter {
                        owner: owner.id,
                        reply,
                        wait,
                    });
                }
            }
            Action::Retry => {
                self.retry();
                if self.cleanup_waiters.len() >= REQUESTS {
                    drop(reply.send(Err(BlobError::Quota)));
                } else {
                    self.cleanup_waiters.push((wait, reply));
                }
            }
            action => {
                let object_receipt = match &action {
                    Action::Append(_, upload, ..) | Action::Commit(_, upload) => Some(upload.id),
                    Action::Transfer(_, blob) => Some(blob.id),
                    _ => None,
                };
                let reader_receipt = if let Action::Read(_, reader) = &action {
                    Some(reader.id)
                } else {
                    None
                };
                let result = self.action(action, &wait).await;
                if result.is_ok() {
                    if let Some(id) = object_receipt
                        && let Some(object) = self.objects.get_mut(&id)
                    {
                        object.receipt = Some(receipt.clone());
                    }
                    if let Some(id) = reader_receipt
                        && let Some(reader) = self.readers.get_mut(&id)
                    {
                        reader.receipt = Some(receipt);
                    }
                }
                drop(reply.send(result));
            }
        }
    }
    fn finish_waiters(&mut self) {
        let mut retained = Vec::new();
        for waiter in self.close_waiters.drain(..) {
            let objects: Vec<_> = self
                .objects
                .values()
                .filter(|o| {
                    o.owner == waiter.owner && !matches!(o.state, ObjectState::Output { .. })
                })
                .collect();
            let result = if let Err(error) = waiter.wait.check() {
                Some(Err(error))
            } else if objects.is_empty() {
                Some(Ok(Reply::Unit))
            } else if objects.iter().all(|o| {
                matches!(
                    o.state,
                    ObjectState::Deleting {
                        in_flight: false,
                        failed: true
                    }
                )
            }) {
                Some(Err(BlobError::Cleanup))
            } else {
                None
            };
            if let Some(result) = result {
                drop(waiter.reply.send(result));
            } else {
                retained.push(waiter);
            }
        }
        self.close_waiters = retained;
        let pending = self.objects.values().any(|o| {
            matches!(
                o.state,
                ObjectState::Deleting {
                    in_flight: true,
                    ..
                } | ObjectState::Deleting { failed: false, .. }
            )
        });
        let failed = self
            .objects
            .values()
            .any(|o| matches!(o.state, ObjectState::Deleting { failed: true, .. }));
        let mut retained = Vec::new();
        for (wait, reply) in self.cleanup_waiters.drain(..) {
            let result = if let Err(error) = wait.check() {
                Some(Err(error))
            } else if !pending {
                Some(if failed {
                    Err(BlobError::Cleanup)
                } else {
                    Ok(Reply::Unit)
                })
            } else {
                None
            };
            if let Some(result) = result {
                drop(reply.send(result));
            } else {
                retained.push((wait, reply));
            }
        }
        self.cleanup_waiters = retained;
    }
}

#[cfg(test)]
impl FilesystemBlobStore {
    pub(in crate::routine::blob) fn faults(&self) -> &Arc<Faults> {
        &self.client.faults
    }
    pub(in crate::routine::blob) fn check_root(
        private: &Path,
        workspace: &Path,
    ) -> Result<(), BlobError> {
        let root = Root::new(private, &WorkspaceRoot::Local(workspace.to_path_buf()))?;
        root.remove_directory()
    }
    pub(in crate::routine::blob) fn check_socket_root(
        private: &Path,
        workspace: &Path,
    ) -> Result<(), BlobError> {
        let root = Root::new(
            private,
            &WorkspaceRoot::SocketWorker(workspace.to_path_buf()),
        )?;
        root.remove_directory()
    }
}
