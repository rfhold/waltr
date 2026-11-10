//! Rust-owned finite media, selected destinations, and shared-client v2 attachments.
use futures_util::{FutureExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use waltr_client::{
    ClientSnapshot, NativeCredentialProvider, NativeGrpcTransport, RoutineDirection,
    RoutineInteractionTransport, RoutineMedia,
    interaction::{self, wire as w},
};
use waltr_desktop::{
    host::Error,
    media::{OutputSink, Source},
    typing::{Guard, LocalFuture},
};
#[cfg(test)]
pub(crate) mod tests;

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
mod playback;
const CHUNK: usize = 65536;
mod download;
const IPC_METADATA_BYTES: usize = 1024 * 1024;
fn bounded_metadata(body: &Value) -> Result<(), String> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("metadata limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(IPC_METADATA_BYTES), body).map_err(|_| failure())
}
fn failure() -> String {
    "Native media operation is unavailable or invalid".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Args {
    pub(crate) generation: u64,
    pub(crate) operation: String,
    #[serde(default)]
    request: Value,
    #[serde(default)]
    handle: String,
    #[serde(default)]
    metadata: String,
    #[serde(default)]
    inputs: Vec<Binding>,
    #[serde(default)]
    outputs: Vec<DestinationBinding>,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    port: String,
    #[serde(default)]
    stream: String,
    #[serde(default)]
    sequence: String,
    #[serde(default)]
    revision: String,
    #[serde(default)]
    next_sequence: u64,
    #[serde(default)]
    invocation: String,
    #[serde(default)]
    selected: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    port: String,
    handle: String,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum DestinationBinding {
    Playback { port: String },
    File { port: String, handle: String },
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PickInput {
    port: String,
    kind: String,
    content_types: Vec<String>,
    max_bytes: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PickOutput {
    port: String,
    suggested_name: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    routine: String,
    revision: String,
    entrypoint: String,
    parameters: waltr_client::RoutineJson,
    correlation: String,
    source_session: Option<String>,
    inputs: Vec<Declaration>,
    selected_text_output: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "format", rename_all = "camelCase", deny_unknown_fields)]
enum Declaration {
    Text {
        port: String,
    },
    File {
        port: String,
        #[serde(rename = "displayName")]
        display_name: String,
        #[serde(rename = "contentType")]
        content_type: String,
        #[serde(rename = "declaredBytes")]
        declared_bytes: u64,
        #[serde(rename = "expectedSha256")]
        expected_sha256: Option<Vec<u8>>,
    },
    Image {
        port: String,
        #[serde(rename = "displayName")]
        display_name: String,
        #[serde(rename = "contentType")]
        content_type: String,
        #[serde(rename = "declaredBytes")]
        declared_bytes: u64,
        #[serde(rename = "expectedSha256")]
        expected_sha256: Option<Vec<u8>>,
        image: String,
    },
}
impl Declaration {
    fn wire(&self) -> Result<w::InputDeclaration, String> {
        let (port, binary) = match self {
            Self::Text { port } => {
                return Ok(w::InputDeclaration {
                    port: port.clone(),
                    format: Some(w::input_declaration::Format::Text(w::TextFormat {})),
                });
            }
            Self::File {
                port,
                display_name,
                content_type,
                declared_bytes,
                expected_sha256,
            } => (
                port,
                w::BinaryInputDeclaration {
                    display_name: display_name.clone(),
                    content_type: content_type.clone(),
                    declared_bytes: Some(*declared_bytes),
                    expected_sha256: expected_sha256.clone(),
                    purpose: Some(w::binary_input_declaration::Purpose::File(
                        w::FilePurpose {},
                    )),
                },
            ),
            Self::Image {
                port,
                display_name,
                content_type,
                declared_bytes,
                expected_sha256,
                image,
            } => (
                port,
                w::BinaryInputDeclaration {
                    display_name: display_name.clone(),
                    content_type: content_type.clone(),
                    declared_bytes: Some(*declared_bytes),
                    expected_sha256: expected_sha256.clone(),
                    purpose: Some(w::binary_input_declaration::Purpose::Image(
                        match image.as_str() {
                            "png" => w::ImageFormat::Png as i32,
                            "jpeg" => w::ImageFormat::Jpeg as i32,
                            "webp" => w::ImageFormat::Webp as i32,
                            _ => return Err(failure()),
                        },
                    )),
                },
            ),
        };
        Ok(w::InputDeclaration {
            port: port.clone(),
            format: Some(w::input_declaration::Format::Binary(binary)),
        })
    }
}

pub(crate) enum Selected {
    Input { file: File, public: Value },
    Output { path: PathBuf },
}
pub(crate) struct Request {
    pub args: Args,
    pub selected: Option<Selected>,
    pub reply: oneshot::Sender<Result<Value, String>>,
    pub ticket: (Option<u64>, u64),
    pub _permit: tokio::sync::OwnedSemaphorePermit,
}
pub(crate) struct RawRequest {
    pub args: Args,
    pub bytes: Vec<u8>,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
    pub _permit: tokio::sync::OwnedSemaphorePermit,
}
pub(crate) fn credit(raw: bool, cancel: bool) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
    static DATA: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    static CONTROL: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    static CANCEL: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let (slot, count) = if cancel {
        (&CANCEL, 1)
    } else if raw {
        (&DATA, 1)
    } else {
        (&CONTROL, 8)
    };
    slot.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(count)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Native client is busy".into())
}
fn picker_credit() -> Result<tokio::sync::OwnedSemaphorePermit, String> {
    static PICKER: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    PICKER
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Native client is busy".into())
}
struct Download {
    grant: Option<interaction::BinaryOutputGrant>,
    owner: Option<download::Owner>,
    descriptor: waltr_client::RoutineBlobDescriptor,
    expires: std::time::Instant,
}
#[derive(Default)]
struct Raw {
    uploads: BTreeMap<String, interaction::BlobUpload>,
    downloads: BTreeMap<String, Download>,
    uncertain: bool,
}
impl Raw {
    async fn close(&mut self) -> bool {
        let mut clean = !self.uncertain;
        for upload in self.uploads.values_mut() {
            if upload.cancel().await.is_err() {
                clean = false;
                self.uncertain = true;
            }
        }
        self.uploads.clear();
        for download in self.downloads.values() {
            if let Some(owner) = &download.owner {
                owner.stop();
            }
        }
        let mut closed = Vec::new();
        for (port, download) in &self.downloads {
            if let Some(owner) = &download.owner
                && !owner.wait_closed().await
            {
                clean = false;
            } else {
                closed.push(port.clone());
            }
        }
        // A bounded wait is not a permanent failure: retain receipts for explicit retry.
        for port in closed {
            self.downloads.remove(&port);
        }
        clean
    }
}
struct Input {
    file: Option<File>,
    public: Value,
}
impl Source for Input {
    fn read(&mut self, max: usize) -> LocalFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            if max == 0 || max > CHUNK {
                return Err(Error::Unsupported);
            }
            let mut bytes = vec![0; max];
            let n = self
                .file
                .as_mut()
                .ok_or(Error::Cancelled)?
                .read(&mut bytes)
                .map_err(|_| Error::Interaction)?;
            bytes.truncate(n);
            Ok(bytes)
        })
    }
    fn finish(&mut self, _success: bool) -> LocalFuture<'_, Result<(), Error>> {
        self.file.take();
        Box::pin(async { Ok(()) })
    }
}
struct Destination {
    path: PathBuf,
    port: String,
    file: Option<File>,
    created: bool,
    complete: bool,
    staging: Option<PathBuf>,
}
impl Destination {
    fn create(&mut self) -> Result<(), Error> {
        let parent = self.path.parent().ok_or(Error::Interaction)?;
        let directory = parent.join(format!(".waltr-{:032x}", rand::random::<u128>()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&directory).map_err(|_| Error::Interaction)?;
        self.staging = Some(directory.clone());
        self.file = Some(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join("content"))
                .map_err(|_| Error::Interaction)?,
        );
        self.created = true;
        Ok(())
    }
    fn publish(&mut self) -> Result<(), Error> {
        let directory = self.staging.as_ref().ok_or(Error::Interaction)?;
        // Atomic no-overwrite publication; cleanup never unlinks the selected path.
        std::fs::hard_link(directory.join("content"), &self.path)
            .map_err(|_| Error::Interaction)?;
        self.complete = true;
        Ok(())
    }
    fn discard(&mut self) {
        self.file.take();
        if let Some(directory) = self.staging.take() {
            let _removed = std::fs::remove_file(directory.join("content"));
            let _removed = std::fs::remove_dir(directory);
        }
        self.created = false;
    }
}
impl Drop for Destination {
    fn drop(&mut self) {
        self.discard();
    }
}
#[derive(Clone)]
struct Fence {
    generation: u64,
    epoch: u64,
    current: Option<Arc<std::sync::atomic::AtomicU64>>,
    audio: Arc<crate::native_audio::Host>,
    credentials: NativeCredentialProvider,
    revision: u64,
    cancel: CancellationToken,
}
impl Fence {
    fn valid(&self) -> bool {
        let state = self.audio.lock();
        state.generation == self.generation
            && self
                .current
                .as_ref()
                .is_none_or(|g| g.load(std::sync::atomic::Ordering::Acquire) == self.generation)
            && state.media_epoch == self.epoch
            && state.allowed
            && !self.cancel.is_cancelled()
            && self.credentials.credential_revision() == self.revision
    }
}
struct Sink {
    files: Mutex<BTreeMap<String, Destination>>,
    file_ports: std::collections::BTreeSet<String>,
    playback_ports: Vec<String>,
    playback_stream: RefCell<Option<String>>,
    transfers: interaction::MediaTransfers,
    readers: Mutex<Vec<interaction::BlobReader>>,
    checkout: Mutex<
        Option<
            futures_util::future::LocalBoxFuture<
                'static,
                Result<interaction::BlobReader, waltr_client::ClientError>,
            >,
        >,
    >,
    events: Rc<RefCell<VecDeque<Value>>>,
    fence: Fence,
    raw: Option<Rc<Mutex<Raw>>>,
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    playback: Mutex<Option<playback::Owner>>,
}
impl OutputSink for Sink {
    fn supports(&self, port: &waltr_client::RoutinePort) -> bool {
        match port.media {
            RoutineMedia::Audio => {
                self.playback_ports.contains(&port.name)
                    && port.direction == RoutineDirection::Output
                    && !port.formats.is_empty()
                    && port.formats.iter().all(|f| f == "pcm16le-24000-mono")
            }
            RoutineMedia::File | RoutineMedia::Image => self.file_ports.contains(&port.name),
            RoutineMedia::Text => true,
        }
    }
    fn write<'a>(
        &'a self,
        frame: waltr_desktop::media::Frame<'a>,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            guard.check().map_err(|_| Error::Cancelled)?;
            match frame {
                waltr_desktop::media::Frame::Blob {
                    port,
                    descriptor,
                    grant,
                } => {
                    if !self.file_ports.contains(port) {
                        if let Some(raw) = &self.raw {
                            let expires = std::time::Instant::now()
                                + std::time::Duration::from_millis(
                                    descriptor
                                        .remaining_validity_ms
                                        .ok_or(Error::Interaction)?
                                        .0
                                        .min(900000),
                                );
                            raw.lock().await.downloads.insert(
                                port.into(),
                                Download {
                                    grant: Some(grant),
                                    owner: None,
                                    descriptor: descriptor.clone(),
                                    expires,
                                },
                            );
                            push_event(
                                &self.events,
                                json!({"type":"blob","port":port,"descriptor":descriptor}),
                            )?;
                        }
                        return Ok(());
                    }
                    let expiry = std::time::Instant::now()
                        + std::time::Duration::from_millis(
                            descriptor
                                .remaining_validity_ms
                                .ok_or(Error::Interaction)?
                                .0
                                .min(900000),
                        );
                    let mut checkout = self.checkout.lock().await;
                    let transfers = self.transfers.clone();
                    *checkout =
                        Some(async move { transfers.read_output(grant).await }.boxed_local());
                    let reader = tokio::select! { biased;
                        () = self.fence.cancel.cancelled() => return Err(Error::Cancelled),
                        () = tokio::time::sleep_until(expiry.into()) => return Err(Error::Interaction),
                        result = checkout.as_mut().ok_or(Error::Interaction)? => result,
                    };
                    checkout.take();
                    drop(checkout);
                    let mut readers = self.readers.lock().await;
                    readers.push(reader.map_err(|_| Error::Interaction)?);
                    let reader = readers.last_mut().ok_or(Error::Interaction)?;
                    if std::time::Instant::now() >= expiry || !self.fence.valid() {
                        return Err(Error::Cancelled);
                    }
                    let mut files = self.files.lock().await;
                    let target = files.get_mut(port).ok_or(Error::Unsupported)?;
                    guard.check().map_err(|_| Error::Cancelled)?;
                    target.create()?;
                    loop {
                        let chunk = tokio::select! { biased;
                            () = self.fence.cancel.cancelled() => return Err(Error::Cancelled),
                            () = tokio::time::sleep_until(expiry.into()) => return Err(Error::Interaction),
                            chunk = reader.next() => chunk,
                        };
                        let Some(chunk) = chunk else {
                            break;
                        };
                        guard.check().map_err(|_| Error::Cancelled)?;
                        if !self.fence.valid() || std::time::Instant::now() >= expiry {
                            return Err(Error::Cancelled);
                        }
                        let bytes = chunk.map_err(|_| Error::Interaction)?;
                        if bytes.len() > CHUNK {
                            return Err(Error::Interaction);
                        }
                        target
                            .file
                            .as_mut()
                            .ok_or(Error::Interaction)?
                            .write_all(&bytes)
                            .map_err(|_| Error::Interaction)?;
                    }
                    if !reader.complete() {
                        return Err(Error::Interaction);
                    }
                    reader.close().await.map_err(|_| Error::Interaction)?;
                    readers.pop();
                    push_event(
                        &self.events,
                        json!({"type":"blob","port":port,"descriptor":descriptor}),
                    )?;
                }
                waltr_desktop::media::Frame::Stream(event) => {
                    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
                    match event {
                        interaction::InteractionEvent::Open(open) => {
                            if let Some(w::stream_open::Format::Pcm16le(format)) = &open.format {
                                if !self.playback_ports.contains(&open.port) {
                                    return Ok(());
                                }
                                let mut playback = self.playback.lock().await;
                                if playback.is_some() {
                                    return Err(Error::Unsupported);
                                }
                                let owner = playback::Owner::start(*format, self.fence.clone())?;
                                *playback = Some(owner);
                                *self.playback_stream.borrow_mut() = Some(open.stream_id.clone());
                            }
                        }
                        interaction::InteractionEvent::Chunk(chunk) => {
                            if let Some(w::stream_chunk::Payload::Audio(bytes)) = &chunk.payload {
                                if self.playback_stream.borrow().as_ref() != Some(&chunk.stream_id)
                                {
                                    return Ok(());
                                }
                                let sender = self
                                    .playback
                                    .lock()
                                    .await
                                    .as_ref()
                                    .ok_or(Error::Unsupported)?
                                    .sender();
                                playback::send(&sender, bytes.clone(), &self.fence).await?;
                            }
                        }
                        interaction::InteractionEvent::Close(close)
                            if self.playback_stream.borrow().as_ref() == Some(&close.stream_id) =>
                        {
                            self.playback_stream.borrow_mut().take();
                            let mut playback = self.playback.lock().await;
                            if let Some(owner) = playback.as_mut() {
                                owner.finish(guard.check().is_ok()).await?;
                            }
                            playback.take();
                        }
                        _ => (),
                    }
                    #[cfg(not(any(
                        target_os = "macos",
                        target_os = "linux",
                        target_os = "windows"
                    )))]
                    {
                        let _event = event;
                        return Err(Error::Unsupported);
                    }
                }
            }
            Ok(())
        })
    }
    fn finish<'a>(
        &'a self,
        success: bool,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut result = Ok(());
            if let Some(checkout) = self.checkout.lock().await.take() {
                // A timed-out checkout may have allocated a remote reader. Retain its
                // future through cleanup; never report a dropped checkout as closed.
                match tokio::time::timeout(std::time::Duration::from_secs(30), checkout).await {
                    Ok(Ok(mut reader)) => {
                        if !matches!(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                reader.close()
                            )
                            .await,
                            Ok(Ok(()))
                        ) {
                            result = Err(Error::Interaction);
                        }
                    }
                    _ => result = Err(Error::Interaction),
                }
            }
            for reader in self.readers.lock().await.iter_mut() {
                if !matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(30), reader.close()).await,
                    Ok(Ok(()))
                ) {
                    result = Err(Error::Interaction);
                }
            }
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            {
                let mut playback = self.playback.lock().await;
                if let Some(owner) = playback.as_mut()
                    && owner
                        .finish(success && guard.check().is_ok())
                        .await
                        .is_err()
                {
                    result = Err(Error::Interaction);
                }
                playback.take();
            }
            let mut files = self.files.lock().await;
            for file in files.values_mut() {
                if success
                    && guard.check().is_ok()
                    && result.is_ok()
                    && let Some(handle) = file.file.take()
                    && handle.sync_all().is_err()
                {
                    result = Err(Error::Interaction);
                }
            }
            for file in files.values_mut() {
                if success
                    && result.is_ok()
                    && guard.check().is_ok()
                    && self.fence.valid()
                    && file.created
                    && file.publish().is_err()
                {
                    result = Err(Error::Interaction);
                }
                file.discard();
            }
            result
        })
    }
}
fn push_event(events: &RefCell<VecDeque<Value>>, event: Value) -> Result<(), Error> {
    let mut events = events.borrow_mut();
    if events.len() >= 32 {
        return Err(Error::Interaction);
    }
    events.push_back(event);
    Ok(())
}
struct Active {
    id: u32,
    fence: Fence,
    input: Option<Rc<Mutex<interaction::AttachmentInput>>>,
    joined: futures_util::future::Shared<futures_util::future::LocalBoxFuture<'static, bool>>,
    uploaded: futures_util::future::Shared<futures_util::future::LocalBoxFuture<'static, bool>>,
    definition: waltr_client::ClientManualRoutineDefinition,
    limits: waltr_client::RoutineLimits,
    source: Option<String>,
    raw: Rc<Mutex<Raw>>,
    raw_mode: bool,
}
#[derive(Clone)]
struct Retired {
    joined: futures_util::future::Shared<futures_util::future::LocalBoxFuture<'static, bool>>,
    raw: Rc<Mutex<Raw>>,
}
struct TextReader {
    reader: Mutex<interaction::DurableTextReader>,
    cancel: CancellationToken,
}
struct State {
    draining: bool,
    next: u32,
    snapshot: ClientSnapshot,
    inputs: BTreeMap<String, Input>,
    outputs: BTreeMap<String, Destination>,
    active: Option<Active>,
    status: Value,
    events: Rc<RefCell<VecDeque<Value>>>,
    text: BTreeMap<u32, Rc<TextReader>>,
    revision: u64,
}
#[derive(Clone)]
pub(crate) struct Host {
    state: Rc<RefCell<State>>,
    transport: NativeGrpcTransport,
    credentials: NativeCredentialProvider,
    audio: Arc<crate::native_audio::Host>,
    generation: u64,
    current: Option<Arc<std::sync::atomic::AtomicU64>>,
    downloads: Rc<RefCell<tokio::task::JoinSet<()>>>,
    retired: Rc<RefCell<Vec<Retired>>>,
    download_join_failed: Rc<std::cell::Cell<bool>>,
}
impl Host {
    pub fn new(
        snapshot: ClientSnapshot,
        transport: NativeGrpcTransport,
        credentials: NativeCredentialProvider,
        audio: Arc<crate::native_audio::Host>,
        generation: u64,
    ) -> Self {
        let revision = credentials.credential_revision();
        Self {
            state: Rc::new(RefCell::new(State {
                draining: false,
                next: 0,
                snapshot,
                inputs: BTreeMap::new(),
                outputs: BTreeMap::new(),
                active: None,
                status: Value::Null,
                events: Rc::default(),
                text: BTreeMap::new(),
                revision,
            })),
            transport,
            credentials,
            audio,
            generation,
            current: None,
            downloads: Rc::default(),
            retired: Rc::default(),
            download_join_failed: Rc::default(),
        }
    }
    pub fn with_current_generation(mut self, current: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.current = Some(current);
        self
    }
    pub fn revoke_if_changed(&self, snapshot: &ClientSnapshot) -> bool {
        let state = self.state.borrow();
        let picker_changed = state.snapshot.routines.definitions != snapshot.routines.definitions
            || state
                .snapshot
                .config
                .current
                .as_ref()
                .map(|c| &c.session_dictation)
                != snapshot
                    .config
                    .current
                    .as_ref()
                    .map(|c| &c.session_dictation)
            || state
                .snapshot
                .sessions
                .current
                .iter()
                .any(|old| !snapshot.sessions.current.iter().any(|new| old.id == new.id));
        let changed = state.revision != self.credentials.credential_revision()
            || if let Some(a) = &state.active {
                !snapshot
                    .routines
                    .definitions
                    .definitions
                    .contains(&a.definition)
                    || snapshot.routines.definitions.limits.as_ref() != Some(&a.limits)
            } else {
                false
            }
            || state.active.as_ref().is_some_and(|a| {
                a.source
                    .as_ref()
                    .is_some_and(|s| !snapshot.sessions.current.iter().any(|v| &v.id == s))
            });
        drop(state);
        if picker_changed {
            let mut audio = self.audio.lock();
            if audio.generation == self.generation {
                audio.picker_epoch = audio.picker_epoch.wrapping_add(1);
            }
        }
        if changed {
            self.revoke();
        }
        changed
    }
    pub fn revoke(&self) {
        let mut state = self.state.borrow_mut();
        state.draining = true;
        {
            let mut audio = self.audio.lock();
            if audio.generation == self.generation {
                audio.access(false);
            }
        }
        if let Some(active) = &state.active {
            active.fence.cancel.cancel();
        }
        for reader in state.text.values() {
            reader.cancel.cancel();
        }
    }
    pub async fn reconcile(&self, snapshot: &ClientSnapshot) {
        let changed = self.revoke_if_changed(snapshot)
            || self.state.borrow().revision != self.credentials.credential_revision()
            || self.state.borrow().active.as_ref().is_some_and(|a| {
                !snapshot
                    .routines
                    .definitions
                    .definitions
                    .contains(&a.definition)
                    || snapshot.routines.definitions.limits.as_ref() != Some(&a.limits)
                    || a.source
                        .as_ref()
                        .is_some_and(|s| !snapshot.sessions.current.iter().any(|v| &v.id == s))
                    || !a.fence.valid()
            });
        if changed {
            self.drain().await;
        }
        let mut state = self.state.borrow_mut();
        state.snapshot = snapshot.clone();
        state.revision = self.credentials.credential_revision();
        state.draining = false;
    }
    pub async fn drain(&self) -> bool {
        self.revoke();
        let readers: Vec<_> = self.state.borrow().text.values().cloned().collect();
        for reader in &readers {
            reader.cancel.cancel();
        }
        if let Some(a) = self.state.borrow_mut().active.take() {
            a.fence.cancel.cancel();
            self.retired.borrow_mut().push(Retired {
                joined: a.joined,
                raw: a.raw,
            });
        }
        let retired = self.retired.borrow().clone();
        let mut clean = true;
        for owner in retired {
            let routine_clean = owner.joined.await;
            let raw_clean = owner.raw.lock().await.close().await;
            if routine_clean && raw_clean {
                self.retired
                    .borrow_mut()
                    .retain(|r| !Rc::ptr_eq(&r.raw, &owner.raw));
            } else {
                clean = false;
            }
        }
        for reader in readers {
            drop(reader.reader.lock().await);
        }
        let mut s = self.state.borrow_mut();
        s.inputs.clear();
        s.outputs.clear();
        s.text.clear();
        s.events.borrow_mut().clear();
        s.status = Value::Null;
        clean && !self.download_join_failed.get()
    }
    pub async fn join_downloads(&self) -> bool {
        let mut tasks = std::mem::take(&mut *self.downloads.borrow_mut());
        let mut clean = !self.download_join_failed.get();
        while let Some(result) = tasks.join_next().await {
            clean &= result.is_ok();
        }
        self.download_join_failed.set(!clean);
        clean
    }
    fn input(&self, id: u32) -> Result<Rc<Mutex<interaction::AttachmentInput>>, String> {
        self.state
            .borrow()
            .active
            .as_ref()
            .filter(|a| a.id == id && a.fence.valid())
            .and_then(|a| a.input.clone())
            .ok_or_else(failure)
    }
    fn check_input(&self, id: u32) -> Result<(), String> {
        self.input(id).map(|_| ())
    }
    pub async fn dispatch(&self, mut request: Request) {
        if request.ticket.1 != self.audio.lock().picker_epoch {
            let _sent = request.reply.send(Err(failure()));
            return;
        }
        let checkout = (request.args.operation == "beginDownload")
            .then(|| (request.args.id, request.args.port.clone()));
        let pending = self.call_inner(request.args, request.selected, Some(request._permit));
        tokio::pin!(pending);
        let result = if checkout.is_some() {
            tokio::select! { biased;
                () = request.reply.closed() => return,
                result = &mut pending => result,
            }
        } else {
            pending.await
        };
        if request.reply.send(result).is_err()
            && let Some((id, port)) = checkout
        {
            let raw = self
                .state
                .borrow()
                .active
                .as_ref()
                .filter(|a| a.id == id)
                .map(|a| a.raw.clone());
            if let Some(raw) = raw {
                let raw = raw.lock().await;
                if let Some(owner) = raw.downloads.get(&port).and_then(|d| d.owner.as_ref()) {
                    owner.stop();
                }
            }
        }
    }
    pub async fn dispatch_raw(&self, mut request: RawRequest) {
        let download = request.args.operation == "download";
        let pending = self.raw_inner(request.args, request.bytes, Some(request._permit));
        tokio::pin!(pending);
        let result = if download {
            tokio::select! { biased;
                () = request.reply.closed() => return,
                result = &mut pending => result,
            }
        } else {
            pending.await
        };
        let _sent = request.reply.send(result);
    }
    fn raw_owner(&self, id: u32) -> Result<(Rc<Mutex<Raw>>, Fence), String> {
        let s = self.state.borrow();
        let a = s
            .active
            .as_ref()
            .filter(|a| a.id == id && a.raw_mode && a.fence.valid())
            .ok_or_else(failure)?;
        Ok((a.raw.clone(), a.fence.clone()))
    }
    #[cfg(test)]
    async fn raw(&self, args: Args, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        self.raw_inner(args, bytes, None).await
    }
    async fn raw_inner(
        &self,
        args: Args,
        bytes: Vec<u8>,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Vec<u8>, String> {
        if args.generation != self.generation {
            return Err(failure());
        }
        let (raw, fence) = self.raw_owner(args.id)?;
        let mut raw = raw.lock().await;
        if !fence.valid() {
            return Err(failure());
        }
        match args.operation.as_str() {
            "upload" => {
                if bytes.is_empty() || bytes.len() > CHUNK {
                    return Err(failure());
                }
                let upload = raw.uploads.get_mut(&args.port).ok_or_else(failure)?;
                let chunk = upload.chunk_bytes() as usize;
                if chunk == 0 {
                    return Err(failure());
                }
                for bytes in bytes.chunks(chunk.min(CHUNK)) {
                    if !fence.valid() {
                        return Err(failure());
                    }
                    tokio::select! { biased; () = fence.cancel.cancelled() => return Err(failure()), result = upload.append(bytes) => result.map_err(|_| failure())? }
                }
                Ok(Vec::new())
            }
            "download" => {
                let download = raw.downloads.get(&args.port).ok_or_else(failure)?;
                if std::time::Instant::now() >= download.expires {
                    return Err(failure());
                }
                let owner = download.owner.clone().ok_or_else(failure)?;
                let expires = download.expires;
                drop(raw);
                let chunk = owner.read(permit).await?;
                let bytes = chunk.bytes;
                let _permit = chunk.permit;
                if bytes.len() > CHUNK || !fence.valid() || std::time::Instant::now() >= expires {
                    return Err(failure());
                }
                Ok(bytes)
            }
            _ => Err(failure()),
        }
    }
    #[cfg(test)]
    async fn call(&self, args: Args, selected: Option<Selected>) -> Result<Value, String> {
        self.call_inner(args, selected, None).await
    }
    async fn call_inner(
        &self,
        args: Args,
        selected: Option<Selected>,
        mut permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Value, String> {
        if args.generation != self.generation
            || self.audio.lock().generation != self.generation
            || self
                .current
                .as_ref()
                .is_some_and(|g| g.load(std::sync::atomic::Ordering::Acquire) != self.generation)
            || self.state.borrow().draining
            || !self.audio.lock().allowed
            || self.state.borrow().revision != self.credentials.credential_revision()
        {
            return Err(failure());
        }
        match args.operation.as_str() {
            "pickInput" => {
                let Some(Selected::Input { file, mut public }) = selected else {
                    return Ok(Value::Null);
                };
                let mut s = self.state.borrow_mut();
                if s.inputs.len() >= 32 {
                    return Err(failure());
                }
                s.next = s.next.checked_add(1).ok_or_else(failure)?;
                let handle = format!("input-{}-{}", self.generation, s.next);
                public["handle"] = json!(handle);
                s.inputs.insert(
                    handle,
                    Input {
                        file: Some(file),
                        public: public.clone(),
                    },
                );
                Ok(public)
            }
            "pickOutput" => {
                let Some(Selected::Output { path }) = selected else {
                    return Ok(Value::Null);
                };
                let request: PickOutput =
                    serde_json::from_value(args.request).map_err(|_| failure())?;
                let mut s = self.state.borrow_mut();
                if s.outputs.len() >= 32 {
                    return Err(failure());
                }
                s.next = s.next.checked_add(1).ok_or_else(failure)?;
                let handle = format!("output-{}-{}", self.generation, s.next);
                s.outputs.insert(
                    handle.clone(),
                    Destination {
                        path,
                        port: request.port.clone(),
                        file: None,
                        created: false,
                        complete: false,
                        staging: None,
                    },
                );
                Ok(json!({"handle":handle,"port":request.port}))
            }
            "releaseInput" => {
                self.state.borrow_mut().inputs.remove(&args.handle);
                Ok(Value::Null)
            }
            "releaseOutput" => {
                self.state.borrow_mut().outputs.remove(&args.handle);
                Ok(Value::Null)
            }
            "openNative" | "open" => self.open(args).await,
            "beginUpload" => {
                let (raw, fence) = self.raw_owner(args.id)?;
                let mut raw = raw.lock().await;
                if !fence.valid() || raw.uploads.contains_key(&args.port) || raw.uploads.len() >= 32
                {
                    return Err(failure());
                }
                let input = self.input(args.id)?;
                let input = input.lock().await;
                if !fence.valid() {
                    return Err(failure());
                }
                let upload = input.begin_upload(&args.port).map_err(|_| failure())?;
                raw.uploads.insert(args.port.clone(), upload);
                let upload = raw.uploads.get_mut(&args.port).ok_or_else(failure)?;
                tokio::select! { biased; () = fence.cancel.cancelled() => return Err(failure()), result = upload.begin() => result.map_err(|_| failure())? }
                Ok(Value::Null)
            }
            "commitUpload" => {
                let (raw, fence) = self.raw_owner(args.id)?;
                let mut raw = raw.lock().await;
                let upload = raw.uploads.get_mut(&args.port).ok_or_else(failure)?;
                if !fence.valid() {
                    return Err(failure());
                }
                let (descriptor, event) = tokio::select! { biased; () = fence.cancel.cancelled() => return Err(failure()), result = upload.commit() => result.map_err(|_| failure())? };
                let input = self.input(args.id)?;
                let mut input = input.lock().await;
                if !fence.valid() {
                    return Err(failure());
                }
                input.send(vec![event]).await.map_err(|_| failure())?;
                raw.uploads.remove(&args.port);
                Ok(json!(
                    serde_json::to_string(&descriptor).map_err(|_| failure())?
                ))
            }
            "beginDownload" => {
                let (raw, fence) = self.raw_owner(args.id)?;
                let mut raw = raw.lock().await;
                let download = raw.downloads.get_mut(&args.port).ok_or_else(failure)?;
                if !fence.valid()
                    || std::time::Instant::now() >= download.expires
                    || download.owner.is_some()
                {
                    return Err(failure());
                }
                let grant = download.grant.take().ok_or_else(failure)?;
                let owner = download::start(
                    download::Start {
                        transfers: self.transport.media_transfers(),
                        grant,
                        fence: fence.clone(),
                        expires: download.expires,
                        activity: self.audio.activity.begin(),
                        permit: permit.take(),
                    },
                    &mut self.downloads.borrow_mut(),
                );
                download.owner = Some(owner.clone());
                let descriptor = download.descriptor.clone();
                drop(raw);
                owner.wait_ready().await?;
                Ok(json!(
                    serde_json::to_string(&descriptor).map_err(|_| failure())?
                ))
            }
            "closeDownload" => {
                let (raw, _) = self.raw_owner(args.id)?;
                let owner = raw
                    .lock()
                    .await
                    .downloads
                    .get(&args.port)
                    .and_then(|d| d.owner.clone());
                if let Some(owner) = owner
                    && !owner.wait_closed().await
                {
                    return Err(failure());
                }
                raw.lock().await.downloads.remove(&args.port);
                Ok(Value::Null)
            }
            "status" => {
                let s = self.state.borrow();
                if s.status["operationId"] != args.id {
                    return Err(failure());
                }
                Ok(json!(s.status.to_string()))
            }
            "outputMetadata" => {
                let s = self.state.borrow();
                if s.status["operationId"] != args.id {
                    return Err(failure());
                }
                Ok(s.events
                    .borrow_mut()
                    .pop_front()
                    .map_or(Value::Null, |e| json!(e.to_string())))
            }
            "cancel" | "dropPlayback" => {
                let join = self
                    .state
                    .borrow()
                    .active
                    .as_ref()
                    .filter(|a| a.id == args.id)
                    .map(|a| {
                        a.fence.cancel.cancel();
                        (a.joined.clone(), a.raw.clone())
                    })
                    .ok_or_else(failure)?;
                let (join, raw) = join;
                let clean = join.await;
                let closed = raw.lock().await.close().await;
                Ok(json!(clean && closed))
            }
            "openText" => {
                let input = self.input(args.id)?;
                let mut input = input.lock().await;
                self.check_input(args.id)?;
                input
                    .send(vec![w::InputEvent {
                        event: Some(w::input_event::Event::Open(w::StreamOpen {
                            stream_id: args.stream,
                            port: args.port,
                            format: Some(w::stream_open::Format::Text(w::TextFormat {})),
                        })),
                    }])
                    .await
                    .map_err(|_| failure())?;
                Ok(Value::Null)
            }
            "text" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Revision {
                    segment: String,
                    text: String,
                    replace: bool,
                    committed: bool,
                }
                if args.revision.len() > CHUNK {
                    return Err(failure());
                }
                let r: Revision = serde_json::from_str(&args.revision).map_err(|_| failure())?;
                let input = self.input(args.id)?;
                let mut input = input.lock().await;
                self.check_input(args.id)?;
                input
                    .send(vec![w::InputEvent {
                        event: Some(w::input_event::Event::Chunk(w::StreamChunk {
                            stream_id: args.stream,
                            sequence: args.sequence.parse().map_err(|_| failure())?,
                            payload: Some(w::stream_chunk::Payload::Text(w::TextRevision {
                                segment_id: r.segment,
                                text: r.text,
                                mutation: if r.replace {
                                    w::TextMutation::Replace as i32
                                } else {
                                    w::TextMutation::Append as i32
                                },
                                committed: r.committed,
                            })),
                        })),
                    }])
                    .await
                    .map_err(|_| failure())?;
                Ok(Value::Null)
            }
            "closeStream" => {
                let input = self.input(args.id)?;
                let mut input = input.lock().await;
                self.check_input(args.id)?;
                input
                    .send(vec![w::InputEvent {
                        event: Some(w::input_event::Event::Close(w::StreamClose {
                            stream_id: args.stream,
                            next_sequence: args.next_sequence,
                        })),
                    }])
                    .await
                    .map_err(|_| failure())?;
                Ok(Value::Null)
            }
            "finishInput" => {
                let uploaded = self
                    .state
                    .borrow()
                    .active
                    .as_ref()
                    .filter(|a| a.id == args.id && a.fence.valid())
                    .map(|a| a.uploaded.clone())
                    .ok_or_else(failure)?;
                if !uploaded.await {
                    return Err(failure());
                }
                let input = self.input(args.id)?;
                let mut input = input.lock().await;
                self.check_input(args.id)?;
                input.finish_input().await.map_err(|_| failure())?;
                Ok(Value::Null)
            }
            "beginTextResult" => {
                let snapshot = self.state.borrow().snapshot.clone();
                let detail = snapshot
                    .routines
                    .details
                    .get(&args.invocation)
                    .and_then(|d| d.detail.as_ref())
                    .ok_or_else(failure)?;
                let reader = self
                    .transport
                    .media_transfers()
                    .text_reader(
                        args.invocation,
                        &detail.summary.text_results,
                        &args.selected,
                    )
                    .map_err(|_| failure())?;
                let mut s = self.state.borrow_mut();
                if s.text.len() >= 32 {
                    return Err(failure());
                }
                s.next = s.next.checked_add(1).ok_or_else(failure)?;
                let id = s.next;
                s.text.insert(
                    id,
                    Rc::new(TextReader {
                        reader: Mutex::new(reader),
                        cancel: CancellationToken::new(),
                    }),
                );
                Ok(json!(id))
            }
            "readTextResult" => {
                let reader = self
                    .state
                    .borrow()
                    .text
                    .get(&args.id)
                    .cloned()
                    .ok_or_else(failure)?;
                let mut inner = reader.reader.lock().await;
                let text = tokio::select! { biased;
                    () = reader.cancel.cancelled() => return Err(failure()),
                    result = inner.next_chunk() => result.map_err(|_| failure())?,
                };
                if !self.audio.lock().allowed
                    || self.state.borrow().revision != self.credentials.credential_revision()
                {
                    return Err(failure());
                }
                Ok(json!(text))
            }
            "closeTextResult" => {
                let reader = self.state.borrow_mut().text.remove(&args.id);
                if let Some(reader) = reader {
                    reader.cancel.cancel();
                    drop(reader.reader.lock().await);
                }
                Ok(Value::Null)
            }
            _ => Err(failure()),
        }
    }
    async fn open(&self, args: Args) -> Result<Value, String> {
        let raw_mode = args.operation == "open";
        if args.metadata.len() > CHUNK {
            return Err(failure());
        }
        let metadata: Metadata = serde_json::from_str(&args.metadata).map_err(|_| failure())?;
        let snapshot = self.state.borrow().snapshot.clone();
        let definition = snapshot
            .routines
            .definitions
            .definitions
            .iter()
            .find(|d| d.routine_name == metadata.routine && d.routine_revision == metadata.revision)
            .ok_or_else(failure)?
            .clone();
        let limits = snapshot
            .routines
            .definitions
            .limits
            .clone()
            .ok_or_else(failure)?;
        if metadata
            .source_session
            .as_ref()
            .is_some_and(|s| !snapshot.sessions.current.iter().any(|v| &v.id == s))
        {
            return Err(failure());
        }
        let invocation = waltr_desktop::interaction::Invocation::new(
            definition.clone(),
            limits.clone().into(),
            metadata.entrypoint,
            metadata.parameters,
            metadata.correlation,
            metadata.source_session.clone(),
        )
        .map_err(|_| failure())?;
        if metadata.selected_text_output.as_ref().is_some_and(|name| {
            !definition.ports.iter().any(|port| {
                &port.name == name
                    && port.direction == RoutineDirection::Output
                    && port.media == RoutineMedia::Text
            })
        }) {
            return Err(failure());
        }
        let declarations = metadata
            .inputs
            .iter()
            .map(Declaration::wire)
            .collect::<Result<Vec<_>, _>>()?;
        let open = w::InteractionOpen {
            selection: Some(invocation.selection),
            inputs: declarations.clone(),
        };
        invocation
            .limits
            .validate_open(&open)
            .map_err(|_| failure())?;
        let previous = self
            .state
            .borrow()
            .active
            .as_ref()
            .map(|a| (a.joined.clone(), a.raw.clone()));
        if let Some((join, raw)) = previous {
            if join.now_or_never().is_none() {
                return Err(failure());
            }
            if let Some(active) = &self.state.borrow().active {
                active.fence.cancel.cancel();
            }
            if !raw.lock().await.close().await {
                return Err(failure());
            }
        }
        let mut state = self.state.borrow_mut();
        if state.draining
            || !self.retired.borrow().is_empty()
            || state.revision != self.credentials.credential_revision()
            || !self.audio.lock().allowed
            || !state
                .snapshot
                .routines
                .definitions
                .definitions
                .contains(&definition)
            || state.snapshot.routines.definitions.limits.as_ref() != Some(&limits)
            || metadata.source_session.as_ref().is_some_and(|source| {
                !state
                    .snapshot
                    .sessions
                    .current
                    .iter()
                    .any(|s| &s.id == source)
            })
            || state
                .active
                .as_ref()
                .is_some_and(|a| a.joined.clone().now_or_never().is_none())
        {
            return Err(failure());
        }
        state.active = None;
        let mut sources = Vec::new();
        let mut used = std::collections::BTreeSet::new();
        let mut input_ports = std::collections::BTreeSet::new();
        for binding in &args.inputs {
            if !used.insert(&binding.handle) || !input_ports.insert(&binding.port) {
                return Err(failure());
            }
            let input = state.inputs.get(&binding.handle).ok_or_else(failure)?;
            let declaration = declarations
                .iter()
                .find(|d| d.port == binding.port)
                .ok_or_else(failure)?;
            let Some(w::input_declaration::Format::Binary(b)) = &declaration.format else {
                return Err(failure());
            };
            if input.public["port"] != binding.port
                || input.public["name"] != b.display_name
                || input.public["contentType"] != b.content_type
                || input.public["bytes"].as_u64() != b.declared_bytes
                || b.expected_sha256.is_some()
                || input.public["purpose"]
                    != if matches!(
                        b.purpose,
                        Some(w::binary_input_declaration::Purpose::Image(_))
                    ) {
                        "image"
                    } else {
                        "file"
                    }
            {
                return Err(failure());
            }
        }
        if declarations
            .iter()
            .filter(|d| matches!(d.format, Some(w::input_declaration::Format::Binary(_))))
            .count()
            != args.inputs.len()
            && !raw_mode
        {
            return Err(failure());
        }
        let mut destinations = BTreeMap::new();
        let mut playback_ports = Vec::new();
        let mut output_ports = std::collections::BTreeSet::new();
        for binding in &args.outputs {
            let (port, media) = match binding {
                DestinationBinding::Playback { port } => (port, RoutineMedia::Audio),
                DestinationBinding::File { port, .. } => (port, RoutineMedia::File),
            };
            if !output_ports.insert(port) {
                return Err(failure());
            }
            let descriptor = definition
                .ports
                .iter()
                .find(|p| &p.name == port && p.direction == RoutineDirection::Output)
                .ok_or_else(failure)?;
            if media == RoutineMedia::Audio {
                if descriptor.media != media || !playback_available() {
                    return Err(failure());
                }
                playback_ports.push(port.clone());
            } else if let DestinationBinding::File { handle, .. } = binding
                && (!matches!(descriptor.media, RoutineMedia::File | RoutineMedia::Image)
                    || state.outputs.get(handle).is_none_or(|d| &d.port != port))
            {
                return Err(failure());
            }
        }
        if definition.ports.iter().any(|p| {
            p.direction == RoutineDirection::Output
                && p.required
                && p.media != RoutineMedia::Text
                && !(raw_mode && matches!(p.media, RoutineMedia::File | RoutineMedia::Image))
                && !output_ports.contains(&p.name)
        }) {
            return Err(failure());
        }
        // Consume opaque handles once, only after the complete selection has passed preflight.
        for binding in args.inputs {
            let input = state.inputs.remove(&binding.handle).ok_or_else(failure)?;
            let declaration = declarations
                .iter()
                .find(|d| d.port == binding.port)
                .ok_or_else(failure)?;
            let Some(w::input_declaration::Format::Binary(b)) = &declaration.format else {
                return Err(failure());
            };
            sources.push((binding.port, b.clone(), input));
        }
        for binding in args.outputs {
            if let DestinationBinding::File { port, handle } = binding {
                destinations.insert(port, state.outputs.remove(&handle).ok_or_else(failure)?);
            }
        }
        state.next = state.next.checked_add(1).ok_or_else(failure)?;
        let id = state.next;
        let fence = Fence {
            generation: self.generation,
            epoch: self.audio.lock().media_epoch,
            current: self.current.clone(),
            audio: self.audio.clone(),
            credentials: self.credentials.clone(),
            revision: self.credentials.credential_revision(),
            cancel: CancellationToken::new(),
        };
        let events = Rc::new(RefCell::new(VecDeque::new()));
        state.events = events.clone();
        state.status =
            json!({"operationId":id,"phase":"opening","preview":"","cleanupVerified":false});
        let (done, joined) = oneshot::channel();
        let joined = joined.map(|r| r.unwrap_or(false)).boxed_local().shared();
        let (uploaded, uploads_done) = oneshot::channel();
        let uploads_done = uploads_done
            .map(|r| r.unwrap_or(false))
            .boxed_local()
            .shared();
        // Reserve before awaiting admission, so simultaneous IPC opens cannot race.
        let host = self.clone();
        let owner_fence = fence.clone();
        let activity = self.audio.activity.begin();
        let raw = Rc::new(Mutex::new(Raw::default()));
        state.active = Some(Active {
            id,
            fence,
            input: None,
            joined,
            uploaded: uploads_done,
            definition,
            limits,
            source: metadata.source_session,
            raw: raw.clone(),
            raw_mode,
        });
        drop(state);
        tokio::task::spawn_local(async move {
            let mut activity = activity;
            let result = host
                .run(
                    id,
                    open,
                    invocation.limits,
                    sources,
                    destinations,
                    playback_ports,
                    metadata.selected_text_output,
                    events,
                    owner_fence,
                    raw_mode.then_some(raw),
                    uploaded,
                )
                .await;
            if host.state.borrow().status["phase"] == "error" {
                activity.fail();
            }
            let _sent = done.send(result);
        });
        Ok(json!(id))
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "one pinned operation owns admission and every cleanup resource"
    )]
    async fn run(
        &self,
        id: u32,
        open: w::InteractionOpen,
        limits: interaction::InteractionLimits,
        mut sources: Vec<(String, w::BinaryInputDeclaration, Input)>,
        destinations: BTreeMap<String, Destination>,
        playback_ports: Vec<String>,
        text_port: Option<String>,
        events: Rc<RefCell<VecDeque<Value>>>,
        fence: Fence,
        raw: Option<Rc<Mutex<Raw>>>,
        uploaded_signal: oneshot::Sender<bool>,
    ) -> bool {
        let accepted_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let session = tokio::select! { biased;
            () = fence.cancel.cancelled() => Err(failure()),
            session = tokio::time::timeout_at(accepted_deadline, self.transport.open_attachment(open, limits.clone())) => session.map_err(|_| failure()).and_then(|s| s.map_err(|_| failure())),
        };
        let Ok(session) = session else {
            for (_, _, source) in &mut sources {
                let _closed = source.finish(false).await;
            }
            self.terminal(id, false, None, false, !fence.valid());
            return false;
        };
        let mut output = session.output;
        let control = session.control;
        let input = Rc::new(Mutex::new(session.input));
        if let Some(active) = self
            .state
            .borrow_mut()
            .active
            .as_mut()
            .filter(|a| a.id == id)
        {
            active.input = Some(input.clone());
        }
        let sink = Sink {
            file_ports: destinations.keys().cloned().collect(),
            files: Mutex::new(destinations),
            playback_ports,
            playback_stream: RefCell::new(None),
            transfers: self.transport.media_transfers(),
            readers: Mutex::new(Vec::new()),
            checkout: Mutex::new(None),
            events,
            fence: fence.clone(),
            raw: raw.clone(),
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            playback: Mutex::new(None),
        };
        let valid = || fence.valid();
        let guard = Guard {
            cancel: &fence.cancel,
            generation: &fence.cancel,
            valid: &valid,
        };
        let mut accepted = false;
        let mut eof = false;
        let mut failed = false;
        let mut terminal_at = None;
        let mut last_renew = std::time::Instant::now();
        let mut uploads = Vec::new();
        let upload_input = input.clone();
        let mut uploading = Box::pin(async {
            // The producer future is polled only after Accepted. Its owned uploads remain
            // outside the future so a cancelled await cannot lose cleanup authority.
            let mut input = upload_input.lock().await;
            for (port, declaration, source) in &mut sources {
                guard.check().map_err(|_| Error::Cancelled)?;
                uploads.push(input.begin_upload(port).map_err(|_| Error::Interaction)?);
                let upload = uploads.last_mut().ok_or(Error::Interaction)?;
                upload.begin().await.map_err(|_| Error::Interaction)?;
                let chunk = (upload.chunk_bytes() as usize).min(CHUNK);
                if chunk == 0 {
                    return Err(Error::Interaction);
                }
                let mut remaining = declaration.declared_bytes.ok_or(Error::Unsupported)?;
                while remaining > 0 {
                    let bytes = source
                        .read(chunk.min(usize::try_from(remaining).unwrap_or(chunk)))
                        .await?;
                    guard.check().map_err(|_| Error::Cancelled)?;
                    if bytes.is_empty() || bytes.len() > chunk {
                        return Err(Error::Interaction);
                    }
                    remaining = remaining
                        .checked_sub(bytes.len() as u64)
                        .ok_or(Error::Interaction)?;
                    upload
                        .append(&bytes)
                        .await
                        .map_err(|_| Error::Interaction)?;
                }
                if !source.read(1).await?.is_empty() {
                    return Err(Error::Interaction);
                }
                let (_, event) = upload.commit().await.map_err(|_| Error::Interaction)?;
                guard.check().map_err(|_| Error::Cancelled)?;
                input
                    .send(vec![event])
                    .await
                    .map_err(|_| Error::Interaction)?;
            }
            Ok::<(), Error>(())
        });
        let mut uploaded = false;
        let mut uploaded_signal = Some(uploaded_signal);
        loop {
            tokio::select! { biased;
                () = fence.cancel.cancelled() => { failed = true; break; }
                () = tokio::time::sleep_until(accepted_deadline), if !accepted => { failed = true; break; }
                () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                    if !fence.valid() { failed = true; break; }
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(i64::MAX, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                    let (attach, lease, absolute) = control.deadlines();
                    if terminal_at.is_some_and(|at: std::time::Instant| at.elapsed().as_secs() >= 30) || now >= absolute || (terminal_at.is_none() && (now >= lease || (!accepted && (now >= attach || tokio::time::Instant::now() >= accepted_deadline)))) { failed = true; break; }
                    if terminal_at.is_none() && last_renew.elapsed().as_millis() >= u128::from((limits.server.attachment_lease_ms / 3).clamp(1, 5000)) {
                        let deadline = if accepted { tokio::time::Instant::now() + std::time::Duration::from_secs(20) } else { accepted_deadline };
                        let renewed = tokio::select! { biased; () = fence.cancel.cancelled() => { failed = true; break; }, result = tokio::time::timeout_at(deadline, control.renew()) => result };
                        if !matches!(renewed, Ok(Ok(()))) { failed = true; break; }
                        last_renew = std::time::Instant::now();
                    }
                }
                result = &mut uploading, if accepted && !uploaded => {
                    uploaded = true;
                    if let Some(signal) = uploaded_signal.take() { let _sent = signal.send(result.is_ok()); }
                    if result.is_err() { failed = true; break; }
                }
                event = output.next() => {
                    let Some(event) = event else { eof = true; break; };
                    let Ok(event) = event else { failed = true; break; };
                    match &event {
                        interaction::InteractionEvent::Accepted { .. } => { accepted = true; self.live(id, &output, text_port.as_deref()); }
                        interaction::InteractionEvent::Terminal { .. } => { terminal_at = Some(std::time::Instant::now()); }
                        interaction::InteractionEvent::Blob { port, descriptor } => {
                            let Some(grant) = output.take_blob_grant(port) else { failed = true; break; };
                            let write = sink.write(waltr_desktop::media::Frame::Blob { port, descriptor, grant }, &guard);
                            if !service_sink(write, &control, &fence, &mut last_renew, limits.server.attachment_lease_ms).await { failed = true; break; }
                        }
                        _ => {
                            let write = sink.write(waltr_desktop::media::Frame::Stream(&event), &guard);
                            if !service_sink(write, &control, &fence, &mut last_renew, limits.server.attachment_lease_ms).await { failed = true; break; }
                            self.live(id, &output, text_port.as_deref());
                        }
                    }
                }
            }
        }
        drop(uploading);
        let cancelled = !fence.valid();
        let success = !failed && eof && uploaded && fence.valid() && output.succeeded();
        if !success {
            fence.cancel.cancel();
        }
        let final_text = if success {
            text_port.as_deref().and_then(|p| output.final_text(p).ok())
        } else {
            None
        };
        // Terminal output may precede the final SendInputBatch response.
        let input_idle = if success {
            tokio::time::timeout(std::time::Duration::from_secs(30), input.lock())
                .await
                .is_ok()
        } else {
            true
        };
        let remote = matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                if success {
                    control.release_and_join().boxed_local()
                } else {
                    control.cancel_and_join().boxed_local()
                }
            )
            .await,
            Ok(Ok(true))
        );
        let mut clean = remote && input_idle;
        if !success {
            for upload in &mut uploads {
                if !matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(30), upload.cancel()).await,
                    Ok(Ok(()))
                ) {
                    clean = false;
                }
            }
        }
        for (_, _, source) in &mut sources {
            if source.finish(success).await.is_err() {
                clean = false;
            }
        }
        if sink.finish(success && clean, &guard).await.is_err() {
            clean = false;
        }
        let routine_clean = clean;
        if !success
            && let Some(raw) = raw
            && !raw.lock().await.close().await
        {
            clean = false;
        }
        self.terminal(
            id,
            success && clean && fence.valid(),
            final_text,
            clean,
            cancelled,
        );
        routine_clean
    }
    fn live(&self, id: u32, output: &interaction::InteractionOutput, port: Option<&str>) {
        let mut state = self.state.borrow_mut();
        if state
            .active
            .as_ref()
            .is_some_and(|a| a.id == id && a.fence.valid())
        {
            state.status = json!({"operationId":id,"phase":"live","preview":port.and_then(|p| output.preview_text(p)).unwrap_or_default(),"cleanupVerified":false});
        }
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "bounded terminal projection has independent cleanup and cancellation outcomes"
    )]
    fn terminal(&self, id: u32, success: bool, text: Option<String>, clean: bool, cancelled: bool) {
        let mut state = self.state.borrow_mut();
        if state.active.as_ref().is_some_and(|a| a.id == id) {
            state.status = json!({"operationId":id,"phase":if success {"succeeded"} else if cancelled {"cancelled"} else {"error"},"preview":"","finalText":if success { text } else {None},"cleanupVerified":clean});
            // Keep the join receipt until explicit cancellation/drain or the next open.
        }
    }
}

async fn service_sink(
    mut write: LocalFuture<'_, Result<(), Error>>,
    control: &interaction::AttachmentControl,
    fence: &Fence,
    last_renew: &mut std::time::Instant,
    lease_ms: u64,
) -> bool {
    loop {
        tokio::select! { biased;
            () = fence.cancel.cancelled() => return false,
            () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .map_or(i64::MAX, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                let (_, lease, absolute) = control.deadlines();
                if !fence.valid() || now >= lease || now >= absolute { return false; }
                if last_renew.elapsed().as_millis() >= u128::from((lease_ms / 3).clamp(1, 5000)) {
                    let remaining = std::time::Duration::from_millis(u64::try_from(absolute.saturating_sub(now)).unwrap_or(0)).min(std::time::Duration::from_secs(20));
                    let renewed = tokio::select! { biased; () = fence.cancel.cancelled() => return false, result = tokio::time::timeout(remaining, control.renew()) => result };
                    if !matches!(renewed, Ok(Ok(()))) { return false; }
                    *last_renew = std::time::Instant::now();
                }
            }
            result = &mut write => return result.is_ok(),
        }
    }
}
fn playback_available() -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        playback::available()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

#[tauri::command]
pub(crate) async fn routine_media_bytes(
    bridge: tauri::State<'_, Arc<crate::bridge::RuntimeBridge>>,
    ipc: tauri::ipc::Request<'_>,
) -> Result<tauri::ipc::Response, String> {
    let tauri::ipc::InvokeBody::Json(body) = ipc.body() else {
        return Err(failure());
    };
    bounded_metadata(body)?;
    let args: Args = serde_json::from_value(body.clone()).map_err(|_| failure())?;
    if args.operation != "download" {
        return Err(failure());
    }
    let generation = args.generation;
    let ticket = bridge.media_ticket(generation)?;
    let bytes = bridge.media_raw(args, Vec::new()).await?;
    if bytes.len() > CHUNK || bridge.media_ticket(generation)? != ticket {
        return Err(failure());
    }
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
pub(crate) async fn routine_media_append(
    bridge: tauri::State<'_, Arc<crate::bridge::RuntimeBridge>>,
    ipc: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = ipc.body() else {
        return Err(failure());
    };
    if bytes.is_empty() || bytes.len() > CHUNK {
        return Err(failure());
    }
    let header = |name| {
        ipc.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(failure)
    };
    if header("x-waltr-operation")? != "upload" {
        return Err(failure());
    }
    let generation: u64 = header("x-waltr-generation")?
        .parse()
        .map_err(|_| failure())?;
    let id: u32 = header("x-waltr-owner")?.parse().map_err(|_| failure())?;
    let encoded = header("x-waltr-port")?;
    if encoded.len() > 384 {
        return Err(failure());
    }
    let encoded = format!("port={encoded}");
    let port = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .ok_or_else(failure)?;
    let args = serde_json::from_value(
        json!({"generation":generation,"id":id,"operation":"upload","port":port}),
    )
    .map_err(|_| failure())?;
    let ticket = bridge.media_ticket(generation)?;
    let _empty = bridge.media_raw(args, bytes.clone()).await?;
    if bridge.media_ticket(generation)? != ticket {
        return Err(failure());
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn routine_media(
    app: tauri::AppHandle,
    bridge: tauri::State<'_, Arc<crate::bridge::RuntimeBridge>>,
    ipc: tauri::ipc::Request<'_>,
) -> Result<Value, String> {
    let tauri::ipc::InvokeBody::Json(body) = ipc.body() else {
        return Err(failure());
    };
    bounded_metadata(body)?;
    let args: Args = serde_json::from_value(body.clone()).map_err(|_| failure())?;
    if !cfg!(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )) {
        return Err("Native routine media is not implemented on this platform".into());
    }
    let ticket = bridge.media_ticket(args.generation)?;
    let _picker = if matches!(args.operation.as_str(), "pickInput" | "pickOutput") {
        Some(picker_credit()?)
    } else {
        None
    };
    let selected = pick(&app, &bridge, &args, ticket, _picker).await?;
    if bridge.media_ticket(args.generation)? != ticket {
        return Err(failure());
    }
    let generation = args.generation;
    let value = bridge.media(args, selected, ticket).await?;
    if bridge.media_ticket(generation)? != ticket {
        return Err(failure());
    }
    bridge.with_current(generation, || {
        Ok(json!({"generation":generation,"value":value}))
    })
}
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn pick(
    app: &tauri::AppHandle,
    bridge: &crate::bridge::RuntimeBridge,
    args: &Args,
    ticket: (Option<u64>, u64),
    pending: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Option<Selected>, String> {
    use tauri_plugin_dialog::DialogExt as _;
    match args.operation.as_str() {
        "pickInput" => {
            let request: PickInput =
                serde_json::from_value(args.request.clone()).map_err(|_| failure())?;
            if request.port.is_empty()
                || request.port.len() > 128
                || !matches!(request.kind.as_str(), "file" | "image")
                || request.content_types.len() > 32
            {
                return Err(failure());
            }
            let max = if request.kind == "file" {
                64 * 1024 * 1024
            } else {
                20 * 1024 * 1024
            };
            if request.max_bytes == 0 || request.max_bytes > max {
                return Err(failure());
            }
            let (send, receive) = oneshot::channel();
            let mut dialog = app.dialog().file();
            if request.kind == "image" {
                dialog = dialog.add_filter("Images", &["png", "jpg", "jpeg", "webp"]);
            }
            dialog.pick_file(move |path| {
                let _pending = pending;
                let _sent = send.send(path);
            });
            let path = receive.await.map_err(|_| failure())?;
            if bridge.media_ticket(args.generation)? != ticket {
                return Err(failure());
            }
            let Some(path) = path else {
                return Ok(None);
            };
            let path = path.into_path().map_err(|_| failure())?;
            let file = File::open(&path).map_err(|_| failure())?;
            let metadata = file.metadata().map_err(|_| failure())?;
            if !metadata.is_file() || metadata.len() > request.max_bytes {
                return Err(failure());
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty() && n.len() <= 256 && !n.chars().any(char::is_control))
                .ok_or_else(failure)?
                .to_owned();
            let content_type = match path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("png") => "image/png",
                Some("jpg" | "jpeg") => "image/jpeg",
                Some("webp") => "image/webp",
                Some("pdf") => "application/pdf",
                Some("txt") => "text/plain",
                _ => "application/octet-stream",
            };
            if request.kind == "image"
                && !matches!(content_type, "image/png" | "image/jpeg" | "image/webp")
            {
                return Err(failure());
            }
            if !request.content_types.is_empty()
                && !request.content_types.iter().any(|m| m == content_type)
            {
                return Err(failure());
            }
            Ok(Some(Selected::Input {
                file,
                public: json!({"port":request.port,"name":name,"contentType":content_type,"bytes":metadata.len(),"purpose":request.kind}),
            }))
        }
        "pickOutput" => {
            let request: PickOutput =
                serde_json::from_value(args.request.clone()).map_err(|_| failure())?;
            if request.port.is_empty()
                || request.port.len() > 128
                || request.suggested_name.len() > 256
                || request.suggested_name.contains(['/', '\\'])
                || request.suggested_name.chars().any(char::is_control)
            {
                return Err(failure());
            }
            let (send, receive) = oneshot::channel();
            app.dialog()
                .file()
                .set_file_name(request.suggested_name)
                .save_file(move |path| {
                    let _pending = pending;
                    let _sent = send.send(path);
                });
            let path = receive.await.map_err(|_| failure())?;
            if bridge.media_ticket(args.generation)? != ticket {
                return Err(failure());
            }
            Ok(path
                .map(|p| {
                    p.into_path()
                        .map(|path| Selected::Output { path })
                        .map_err(|_| failure())
                })
                .transpose()?)
        }
        _ => Ok(None),
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn pick(
    _app: &tauri::AppHandle,
    _bridge: &crate::bridge::RuntimeBridge,
    args: &Args,
    _ticket: (Option<u64>, u64),
    _pending: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Option<Selected>, String> {
    if matches!(args.operation.as_str(), "pickInput" | "pickOutput") {
        return Err(failure());
    }
    Ok(None)
}
