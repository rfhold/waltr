//! Host-owned media operations over the ordinary invocation and helper services.
#[cfg(test)]
#[path = "script_routine_media_tests.rs"]
mod tests;
#[cfg(test)]
static BLOB_READER_OPENS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
use crate::routine::{
    blob::{self, BlobStore},
    definition::{Definition, Direction, Media},
    history::{Caller, InvocationBinding},
    media::{
        self,
        snapshot::{Workspace, WorkspacePool},
        speech::SpeechBackend,
        stream::{self, Chunk, Format, Reader, Streams, Text, Writer},
    },
    ports::{
        HelperPayload, HelperRequest, HelperResult, HelperRunner, HostFactory, OperationPolicy,
        PortError, PortFuture, PreparedHost, Reservation, SelectedEntrypoint,
    },
    text_results::{InvocationTextScope, TextResultStore, TextSource, TextSourceFuture},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use waltr_agent::{
    CompletionDecision, CompletionObserver, CompletionObserverError, CompletionObserverFuture,
    CompletionProposal, TransientInputBudget, TransientInputKind, TransientModelCapabilities,
};
use waltr_script::{
    CleanupFuture, HostContext, HostDispatcher, HostFuture, HostHandle, HostReply, HostRequest,
    InvocationId, Limits, StructuredValue,
};

/// Bounded upload source supplied by an authenticated transport, never a guest path.
pub trait UploadSource: Send + Sync {
    /// At most 64 KiB; None means explicit successful input completion.
    fn next(&mut self, cancellation: CancellationToken) -> PortFuture<Option<Vec<u8>>>;
    /// Cancel and join transport-owned producers, including after a failed upload.
    fn close(&mut self) -> PortFuture<()>;
}
/// Cold authenticated transport owner; startup occurs only after SQL admission.
pub trait MediaDriver: Send + Sync {
    /// Cold declaration of a transport-owned repeated audio input, not ambient access.
    fn repeated_input_format(&self, _port: &str) -> Option<Format> {
        None
    }
    /// Take the next distinctly identified capture from an opt-in repeated input.
    fn next_capture(&self, _port: String) -> PortFuture<Option<(String, Reader)>> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Resolve a consumed capture only after its derived prompt has been submitted.
    fn acknowledge_capture(&self, _id: String) -> PortFuture<()> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Atomically stop admission, unless an accepted capture remains unresolved.
    fn seal_captures(&self) -> PortFuture<bool> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Bind transport uploads to the actual service-issued owner during preparation.
    fn bind_blobs(
        &self,
        _binding: InvocationBinding,
        _owner: blob::InvocationOwner,
    ) -> Result<(), PortError> {
        Ok(())
    }
    /// Publish provenance only after the ordinary helper has returned its actual result.
    fn record_helper(&self, _session: String, _definition: String) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
    /// Transfer a declared binary output to the authenticated transport before teardown.
    fn record_binary(&self, _port: String, _lease: blob::OutputLease) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
    /// Reuse an already quarantined-and-committed input in this same trusted blob scope.
    fn committed_input(&self, _port: String) -> PortFuture<Option<blob::CommittedBlob>> {
        Box::pin(async { Ok(None) })
    }
    /// Start all owned input producers and required output consumers exactly once.
    fn start(&self) -> PortFuture<()>;
    /// Cancel blocked transport work; cancellation is not successful completion.
    fn cancel(&self);
    /// Join all transport work and prove explicit input completion and output EOF.
    fn finish(&self) -> PortFuture<()>;
    /// Join cleanup, including intentionally cancelled work; never detach a worker.
    fn join(&self) -> PortFuture<()>;
}
/// Quarantined input. The store validates the entire object before helper access.
pub struct BlobInput {
    /// Immutable declared type and size checked during preflight.
    pub metadata: blob::UploadMetadata,
    /// Authenticated input owner, retained until joined cleanup.
    pub source: Box<dyn UploadSource>,
}
/// Explicit shared server result credit; inject the same pools across invocations.
#[derive(Clone, Debug)]
pub struct ResultPool(Arc<Semaphore>, usize);
impl ResultPool {
    /// Shared 64 MiB per-principal server result capacity.
    pub fn principal() -> Self {
        Self(Arc::new(Semaphore::new(64 * 1024 * 1024)), 64 * 1024 * 1024)
    }
    /// Shared 128 MiB process server result capacity.
    pub fn process() -> Self {
        Self(
            Arc::new(Semaphore::new(128 * 1024 * 1024)),
            128 * 1024 * 1024,
        )
    }
    /// Reserved bytes, excluding the ordinary agent's own state.
    pub fn retained(&self) -> usize {
        self.1 - self.0.available_permits()
    }
}
/// Authenticated transport and adapter composition. Construction performs no effects.
pub struct MediaBindings {
    /// Owns transport tasks through successful completion or joined cancellation.
    pub driver: Arc<dyn MediaDriver>,
    /// Host-pinned diarization requirement, checked before admission.
    pub snapshot: bool,
    /// Must equal the service-issued binding principal.
    pub principal: String,
    /// One invocation's typed streams, with injected shared principal/process pools.
    pub streams: Streams,
    /// Host-constructed input edges by exact declared port name.
    pub inputs: BTreeMap<String, Reader>,
    /// Host-constructed required output edges by exact declared port name.
    pub outputs: BTreeMap<String, Writer>,
    /// Finite upload owners by exact declared port name.
    pub uploads: BTreeMap<String, BlobInput>,
    /// Pinned speech backend; validates formats without connecting at composition.
    pub speech: Arc<dyn SpeechBackend>,
    /// Normal agent execution, not a provider loop.
    pub helpers: Arc<dyn HelperRunner>,
    /// Optional invocation-local authorized source projections.
    pub context: Option<Arc<dyn crate::routine::context::ContextPort>>,
    /// Shared blob service with encoded-byte, MIME, and fixed signature checks.
    pub blobs: Arc<dyn BlobStore>,
    /// Selected durable business results.
    pub text: Arc<dyn TextResultStore>,
    /// Explicit global attachment credit.
    pub attachments: TransientInputBudget,
    /// Shared speech workspace pools.
    pub workspace_principal: WorkspacePool,
    /// Shared speech workspace process pool.
    pub workspace_process: WorkspacePool,
    /// Shared server-owned result capacity.
    pub result_principal: ResultPool,
    /// Shared server-owned result process capacity.
    pub result_process: ResultPool,
    /// Authenticated output lease handoff; at most 32 objects per invocation.
    pub leases: Arc<Mutex<Vec<blob::OutputLease>>>,
}
/// Replaceable authenticated composition, called before any durable admission/effect.
pub trait MediaBindingFactory: Send + Sync {
    /// Pin capabilities, model/API, speech, and transport port metadata without effects.
    fn bind(
        &self,
        binding: InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<MediaBindings>;
}
/// Media composition over the shared invocation service and engine.
pub struct MediaHostFactory {
    /// Explicit authenticated adapter constructor.
    pub bindings: Arc<dyn MediaBindingFactory>,
}
impl std::fmt::Debug for BlobInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobInput")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for MediaBindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaBindings")
            .field("input_count", &self.inputs.len())
            .field("output_count", &self.outputs.len())
            .field("upload_count", &self.uploads.len())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for MediaHostFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaHostFactory").finish_non_exhaustive()
    }
}
impl HostFactory for MediaHostFactory {
    fn preflight(
        &self,
        binding: InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<PreparedHost> {
        let factory = self.bindings.clone();
        Box::pin(async move {
            if binding.principal() != caller.principal {
                return Err(PortError::Denied);
            }
            let mut bindings = factory
                .bind(binding.clone(), definition.clone(), selected, caller)
                .await?;
            validate_bindings(&bindings, &binding, &definition)?;
            if definition
                .envelope()
                .spec
                .capabilities
                .contains("media.inputs")
            {
                bindings
                    .streams
                    .enable_repeated_inputs()
                    .map_err(|_| PortError::Invalid)?;
            }
            bindings
                .helpers
                .bind_captures(definition.envelope().spec.captures.clone())
                .await?;
            bindings
                .helpers
                .pin_named(
                    definition
                        .envelope()
                        .spec
                        .capabilities
                        .iter()
                        .filter_map(|cap| cap.strip_prefix("helper.named.").map(str::to_owned))
                        .collect(),
                )
                .await?;
            if !definition.envelope().spec.captures.is_empty() {
                bindings.helpers.preflight_captures().await?;
            }
            let tool_streams = super::tool_streams::ToolStreams::new(
                definition.envelope().spec.tool_streams.clone(),
            );
            if !definition.envelope().spec.tool_streams.is_empty() {
                bindings
                    .helpers
                    .bind_tool_streams(tool_streams.clone())
                    .await?;
            }
            let cancel = bindings.streams.cancellation();
            let upload_owners: Vec<_> = std::mem::take(&mut bindings.uploads)
                .into_iter()
                .map(|(name, upload)| (name, Arc::new(tokio::sync::Mutex::new(Some(upload)))))
                .collect();
            let state = State {
                inputs: std::mem::take(&mut bindings.inputs),
                outputs: std::mem::take(&mut bindings.outputs),
                uploads: upload_owners.iter().cloned().collect(),
                upload_owners: upload_owners.into_iter().map(|(_, owner)| owner).collect(),
                binary_outputs: definition
                    .envelope()
                    .spec
                    .ports
                    .iter()
                    .filter(|(_, port)| {
                        matches!(port.direction, Direction::Output)
                            && matches!(port.media, Media::Image | Media::File)
                    })
                    .map(|(name, _)| (name.clone(), ()))
                    .collect(),
                ..State::default()
            };
            let host = Arc::new(MediaHost {
                binding,
                definition,
                bindings: Arc::new(bindings),
                state: Arc::new(Mutex::new(state)),
                cancel,
                jobs: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                startup: Arc::new(tokio::sync::Mutex::new(false)),
                result_credit: Arc::new(Semaphore::new(64 * 1024 * 1024)),
                cleanup: Arc::new(tokio::sync::Mutex::new(Cleanup::default())),
                runtime: tokio::runtime::Handle::current(),
                tool_streams,
            });
            Ok(PreparedHost {
                dispatcher: host.clone(),
                policy: host.clone(),
                reservation: Box::new(MediaReservation(host)),
                snapshot: None,
            })
        })
    }
}

fn validate_bindings(
    b: &MediaBindings,
    binding: &InvocationBinding,
    definition: &Definition,
) -> Result<(), PortError> {
    if b.principal != binding.principal()
        || b.context.is_none()
            && definition
                .envelope()
                .spec
                .capabilities
                .iter()
                .any(|cap| cap.starts_with("context."))
        || !b
            .leases
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .is_empty()
        || definition.envelope().spec.capabilities.iter().any(|c| {
            c != "media.execute"
                && c != "media.inputs"
                && c != "helper.run"
                && c != "helper.validate"
                && c != "helper.streams"
                && !c.starts_with("helper.named.")
                && !c.starts_with("helper.restricted.")
                && !c.starts_with("helper.capture.")
                && !crate::routine::context::CAPABILITIES.contains(&c.as_str())
        })
        || !definition
            .envelope()
            .spec
            .capabilities
            .contains("media.execute")
    {
        return Err(PortError::Denied);
    }
    for (name, port) in &definition.envelope().spec.ports {
        let format = match port.media {
            Media::Audio => Some(Format::Pcm16Le {
                rate: 24000,
                channels: 1,
            }),
            Media::Text => Some(Format::Text),
            _ => None,
        };
        let allowed_format = match port.media {
            Media::Audio => "pcm16le-24000-mono",
            Media::Text => "utf8",
            Media::Image | Media::File => "blob",
        };
        if !port.formats.iter().any(|format| format == allowed_format) {
            return Err(PortError::UnsupportedCapability);
        }
        let present = match (port.direction, format) {
            (Direction::Input, Some(format)) => {
                b.inputs.get(name).is_some_and(|r| {
                    r.format() == format
                        && r.direction() == stream::Direction::Input
                        && b.streams.owns_reader(r)
                }) || definition
                    .envelope()
                    .spec
                    .capabilities
                    .contains("media.inputs")
                    && b.driver.repeated_input_format(name) == Some(format)
            }
            (Direction::Output, Some(format)) => b.outputs.get(name).is_some_and(|w| {
                w.format() == format
                    && w.direction() == stream::Direction::Output
                    && b.streams.owns_writer(w)
            }),
            (Direction::Input, None) => b.uploads.get(name).is_some_and(|input| {
                input
                    .metadata
                    .declared_bytes
                    .is_some_and(|bytes| bytes <= u64::from(port.max_bytes))
            }),
            (Direction::Output, None) => true,
        };
        if port.required && !present {
            return Err(PortError::UnsupportedCapability);
        }
        if !present
            && (b.inputs.contains_key(name)
                || b.outputs.contains_key(name)
                || b.uploads.contains_key(name))
        {
            return Err(PortError::Invalid);
        }
        if let Some(reader) = b.inputs.get(name) {
            reader
                .restrict_bytes(port.max_bytes as usize)
                .map_err(|_| PortError::Invalid)?;
        }
        if let Some(writer) = b.outputs.get(name) {
            writer
                .restrict_bytes(port.max_bytes as usize)
                .map_err(|_| PortError::Invalid)?;
        }
        if matches!(port.media, Media::Image)
            && b.uploads
                .get(name)
                .is_some_and(|input| !matches!(input.metadata.kind, blob::BlobKind::Image(_)))
        {
            return Err(PortError::Invalid);
        }
    }
    if b.inputs
        .keys()
        .chain(b.outputs.keys())
        .chain(b.uploads.keys())
        .any(|name| !definition.envelope().spec.ports.contains_key(name))
    {
        return Err(PortError::Invalid);
    }
    Ok(())
}
type Job = Arc<
    tokio::sync::Mutex<Option<tokio::task::JoinHandle<Result<HostReply, waltr_script::Error>>>>,
>;
#[derive(Default)]
struct State {
    engine: Option<InvocationId>,
    inputs: BTreeMap<String, Reader>,
    outputs: BTreeMap<String, Writer>,
    uploads: BTreeMap<String, Arc<tokio::sync::Mutex<Option<BlobInput>>>>,
    upload_owners: Vec<Arc<tokio::sync::Mutex<Option<BlobInput>>>>,
    cleanup_failed: bool,
    binary_outputs: BTreeMap<String, ()>,
    blob_owner: Option<blob::InvocationOwner>,
    resources: Vec<(HostHandle, Resource)>,
    proposals: BTreeMap<String, Arc<CompletionProposal>>,
    prepared: bool,
    cleaned: bool,
    active: usize,
    finishing: bool,
    finished: bool,
}
#[derive(Clone)]
enum Resource {
    Target {
        target: Arc<dyn crate::routine::ports::HelperTarget>,
        credit: Arc<Vec<OwnedSemaphorePermit>>,
        completed: Arc<AtomicBool>,
    },
    Capture(Arc<Capture>),
    TextFeed(Arc<TextFeed>),
    Text {
        text: Arc<String>,
        helper_session: Option<String>,
        _credit: Arc<Vec<OwnedSemaphorePermit>>,
    },
    Helper {
        result: Arc<HelperResult>,
        _credit: Arc<Vec<OwnedSemaphorePermit>>,
    },
    Blob(blob::CommittedBlob),
}
struct TextFeed {
    writer: tokio::sync::Mutex<Option<(Writer, u64)>>,
    reader: Mutex<Option<Reader>>,
}
struct Capture {
    id: String,
    format: Format,
    reader: Mutex<Option<Reader>>,
    collected: AtomicBool,
}
#[derive(Clone)]
struct MediaHost {
    tool_streams: Arc<super::tool_streams::ToolStreams>,
    cleanup: Arc<tokio::sync::Mutex<Cleanup>>,
    runtime: tokio::runtime::Handle,
    binding: InvocationBinding,
    definition: Arc<Definition>,
    bindings: Arc<MediaBindings>,
    state: Arc<Mutex<State>>,
    jobs: Arc<tokio::sync::Mutex<Vec<Job>>>,
    startup: Arc<tokio::sync::Mutex<bool>>,
    cancel: CancellationToken,
    result_credit: Arc<Semaphore>,
}
#[derive(Default)]
struct Cleanup {
    task: Option<tokio::task::JoinHandle<Result<(), PortError>>>,
    result: Option<Result<(), PortError>>,
}
impl MediaHost {
    fn scope(&self) -> blob::BlobScope {
        blob::BlobScope {
            principal: self.binding.principal().into(),
            invocation: self.binding.id().into(),
        }
    }
    fn wait(&self) -> blob::Wait {
        blob::Wait {
            cancellation: self.cancel.clone(),
            deadline: self.bindings.streams.deadline(),
        }
    }
    fn charge(&self, bytes: usize) -> Result<Arc<Vec<OwnedSemaphorePermit>>, waltr_script::Error> {
        let mut permits = Vec::new();
        for pool in [
            &self.result_credit,
            &self.bindings.result_principal.0,
            &self.bindings.result_process.0,
        ] {
            permits.push(
                pool.clone()
                    .try_acquire_many_owned(
                        u32::try_from(bytes).map_err(|_| waltr_script::Error::Limit)?,
                    )
                    .map_err(|_| waltr_script::Error::Limit)?,
            );
        }
        Ok(Arc::new(permits))
    }
    fn resource(
        &self,
        request: &HostRequest,
        context: &HostContext,
    ) -> Result<Resource, waltr_script::Error> {
        let handle = request
            .handle
            .as_ref()
            .ok_or(waltr_script::Error::Identity)?;
        context.validate_handle(handle, handle.kind())?;
        self.state
            .lock()
            .map_err(|_| waltr_script::Error::Cleanup)?
            .resources
            .iter()
            .find(|(h, _)| h == handle)
            .map(|(_, r)| r.clone())
            .ok_or(waltr_script::Error::Identity)
    }
    fn issue(
        &self,
        context: &HostContext,
        resource: Resource,
    ) -> Result<HostReply, waltr_script::Error> {
        let kind = match resource {
            Resource::Target { .. } => "helper.target",
            Resource::Capture(_) => "media.capture",
            Resource::TextFeed(_) => "media.textFeed",
            Resource::Text { .. } => "media.text",
            Resource::Helper { .. } => "helper.result",
            Resource::Blob(..) => "media.blob",
        };
        let handle = context.issue_handle(kind)?;
        self.state
            .lock()
            .map_err(|_| waltr_script::Error::Cleanup)?
            .resources
            .push((handle.clone(), resource));
        Ok(HostReply::Handle(handle))
    }
    fn check(
        &self,
        request: &HostRequest,
        context: &HostContext,
    ) -> Result<Arguments, waltr_script::Error> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| waltr_script::Error::Cleanup)?;
        if state.cleaned
            || !state.prepared
            || self.cancel.is_cancelled()
            || state.engine.is_some_and(|id| id != context.invocation())
        {
            return Err(waltr_script::Error::Identity);
        }
        if state.finishing || state.finished {
            return Err(waltr_script::Error::Denied);
        }
        state.engine = Some(context.invocation());
        drop(state);
        if request.bytes.is_some() {
            return Err(waltr_script::Error::Invalid);
        }
        let mut args: Arguments = if request.operation.starts_with("context.") {
            if !self
                .definition
                .envelope()
                .spec
                .capabilities
                .contains(&request.operation)
                || request.handle.is_some()
                || request.callback.is_some()
            {
                return Err(waltr_script::Error::Denied);
            }
            let operation = crate::routine::context::ContextOperation::parse(
                &request.operation,
                request.arguments.clone().into_json(),
            )
            .map_err(|_| waltr_script::Error::Invalid)?;
            self.bindings
                .context
                .as_ref()
                .ok_or(waltr_script::Error::Denied)?
                .check(&operation)
                .map_err(|_| waltr_script::Error::Denied)?;
            Arguments {
                context_operation: Some(operation),
                ..Arguments::default()
            }
        } else if matches!(request.operation.as_str(), "helper.run" | "helper.open") {
            let helper: super::authorized_tools::HelperArguments =
                serde_json::from_value(request.arguments.clone().into_json())
                    .map_err(|_| waltr_script::Error::Invalid)?;
            helper
                .validate(
                    &self.definition.envelope().spec.capabilities,
                    request.callback.is_some(),
                )
                .map_err(|_| waltr_script::Error::Denied)?;
            if request.operation == "helper.open"
                && (request.handle.is_some()
                    || helper.source.is_some()
                    || helper.field.is_some()
                    || request.callback.is_some())
            {
                return Err(waltr_script::Error::Invalid);
            }
            if let Some(source) = &helper.source {
                self.bindings
                    .context
                    .as_ref()
                    .ok_or(waltr_script::Error::Denied)?
                    .resolve_source(source)
                    .map_err(|_| waltr_script::Error::Denied)?;
            }
            Arguments {
                helper_runner: self
                    .bindings
                    .helpers
                    .select_inference(&helper.inference)
                    .map_err(|_| waltr_script::Error::Invalid)?,
                helper: Some(helper),
                ..Arguments::default()
            }
        } else {
            serde_json::from_value(request.arguments.clone().into_json())
                .map_err(|_| waltr_script::Error::Invalid)?
        };
        match request.operation.as_str() {
            "helper.submit" | "helper.idle" | "helper.seal" => {
                if !self
                    .definition
                    .envelope()
                    .spec
                    .capabilities
                    .contains("helper.run")
                    || request.callback.is_some()
                    || !matches!(self.resource(request, context)?, Resource::Target { .. })
                {
                    return Err(waltr_script::Error::Denied);
                }
                let keys = request
                    .arguments
                    .as_json()
                    .as_object()
                    .ok_or(waltr_script::Error::Invalid)?;
                if request.operation == "helper.submit" {
                    if keys.len() != 1
                        || args
                            .task
                            .as_ref()
                            .is_none_or(|task| task.trim().is_empty() || task.len() > 16384)
                    {
                        return Err(waltr_script::Error::Invalid);
                    }
                } else if !keys.is_empty() {
                    return Err(waltr_script::Error::Invalid);
                }
            }
            "media.input.next" | "media.input.seal" | "media.input.acknowledge" => {
                if !self
                    .definition
                    .envelope()
                    .spec
                    .capabilities
                    .contains("media.inputs")
                    || request.callback.is_some()
                {
                    return Err(waltr_script::Error::Denied);
                }
                match request.operation.as_str() {
                    "media.input.acknowledge" => {
                        if !matches!(self.resource(request, context)?, Resource::Capture(_)) {
                            return Err(waltr_script::Error::Identity);
                        }
                    }
                    "media.input.next" => {
                        if request.handle.is_some()
                            || self
                                .bindings
                                .driver
                                .repeated_input_format(required(&args.port)?)
                                .is_none()
                        {
                            return Err(waltr_script::Error::Identity);
                        }
                    }
                    _ => {
                        if request.handle.is_some() {
                            return Err(waltr_script::Error::Invalid);
                        }
                    }
                }
            }
            "media.collect" if request.handle.is_some() => {
                if args.port.is_some()
                    || request.callback.is_some()
                    || !matches!(self.resource(request, context)?, Resource::Capture(_))
                {
                    return Err(waltr_script::Error::Invalid);
                }
            }
            "helper.next" => {
                if !self
                    .definition
                    .envelope()
                    .spec
                    .capabilities
                    .contains("helper.streams")
                    || request.handle.is_some()
                    || request.callback.is_some()
                {
                    return Err(waltr_script::Error::Denied);
                }
                self.tool_streams
                    .check(required(&args.stream)?)
                    .map_err(|_| waltr_script::Error::Denied)?;
                serde_json::from_value::<super::tool_streams::Next>(
                    request.arguments.clone().into_json(),
                )
                .map_err(|_| waltr_script::Error::Invalid)?;
            }
            "media.synthesize" if request.handle.is_some() => {
                if !matches!(self.resource(request, context)?, Resource::TextFeed(_))
                    || request.callback.is_some()
                {
                    return Err(waltr_script::Error::Identity);
                }
            }
            "media.text.write" | "media.text.close" => {
                if !matches!(self.resource(request, context)?, Resource::TextFeed(_))
                    || request.callback.is_some()
                {
                    return Err(waltr_script::Error::Identity);
                }
                if request.operation == "media.text.write"
                    && args.text.as_ref().is_none_or(|s| s.len() > 65500)
                {
                    return Err(waltr_script::Error::Invalid);
                }
                let raw = request.arguments.as_json();
                let keys = raw.as_object().ok_or(waltr_script::Error::Invalid)?;
                if keys.len() != usize::from(request.operation == "media.text.write")
                    || keys.keys().any(|key| key != "text")
                {
                    return Err(waltr_script::Error::Invalid);
                }
            }
            "media.text.open" => {
                if request.handle.is_some()
                    || request.callback.is_some()
                    || !request
                        .arguments
                        .as_json()
                        .as_object()
                        .is_some_and(|args| args.is_empty())
                {
                    return Err(waltr_script::Error::Invalid);
                }
            }
            "media.finish" | "media.transcribe" | "media.collect" | "media.synthesize"
            | "media.upload" | "media.tee" => {
                if request.handle.is_some() || request.callback.is_some() {
                    return Err(waltr_script::Error::Invalid);
                }
            }
            "helper.run" | "helper.open" => {
                if request.handle.is_some() {
                    self.resource(request, context)?;
                }
            }
            "media.select" | "media.publish" | "media.emit" | "media.lease" | "media.read"
            | "media.readText" => {
                self.resource(request, context)?;
            }
            "media.proposal.read" => {
                if request.handle.is_some() || request.callback.is_some() {
                    return Err(waltr_script::Error::Invalid);
                }
            }
            name if name.starts_with("context.") && args.context_operation.is_some() => {}
            _ => return Err(waltr_script::Error::Denied),
        }
        if request.operation != "helper.run" && request.callback.is_some() {
            return Err(waltr_script::Error::Invalid);
        }
        if request.operation == "media.synthesize" {
            if args.utterances.is_some() && request.handle.is_none() {
                return Err(waltr_script::Error::Invalid);
            }
            let raw = request.arguments.clone().into_json();
            if request.handle.is_some() && (args.port.is_some() || args.text.is_some()) {
                return Err(waltr_script::Error::Invalid);
            }
            match (raw.get("port"), raw.get("text")) {
                (None, None) if request.handle.is_some() => {}
                (Some(Value::String(_)), None) => {}
                (None, Some(Value::String(text))) if text.len() <= 1048576 => {}
                (None, Some(Value::String(_))) => return Err(waltr_script::Error::Limit),
                _ => return Err(waltr_script::Error::Invalid),
            }
            let state = self
                .state
                .lock()
                .map_err(|_| waltr_script::Error::Cleanup)?;
            let output = state
                .outputs
                .get(required(&args.output)?)
                .ok_or(waltr_script::Error::Identity)?;
            if output.format()
                != (Format::Pcm16Le {
                    rate: 24000,
                    channels: 1,
                })
            {
                return Err(waltr_script::Error::Invalid);
            }
        } else if args.utterances.is_some()
            || args.text.is_some() && request.operation != "media.text.write"
        {
            return Err(waltr_script::Error::Invalid);
        }
        if matches!(
            request.operation.as_str(),
            "media.collect" | "media.transcribe" | "media.synthesize"
        ) {
            let format = if request.operation == "media.collect" && request.handle.is_some() {
                let Resource::Capture(capture) = self.resource(request, context)? else {
                    return Err(waltr_script::Error::Identity);
                };
                capture.format
            } else if args.text.is_some()
                || request.operation == "media.synthesize" && request.handle.is_some()
            {
                Format::Text
            } else {
                self.state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .inputs
                    .get(required(&args.port)?)
                    .ok_or(waltr_script::Error::Identity)?
                    .format()
            };
            if request.operation != "media.collect" || format != Format::Text {
                let backend = self
                    .bindings
                    .speech
                    .select(
                        &args.speech.clone().unwrap_or_default(),
                        request.operation == "media.synthesize",
                        args.snapshot == Some(true),
                    )
                    .map_err(|_| waltr_script::Error::Invalid)?
                    .unwrap_or_else(|| self.bindings.speech.clone());
                backend
                    .preflight(format, args.snapshot == Some(true))
                    .map_err(|_| waltr_script::Error::Invalid)?;
                args.backend = Some(backend);
            } else if args.speech.is_some() || args.snapshot == Some(true) {
                return Err(waltr_script::Error::Invalid);
            }
        } else if args.speech.is_some() {
            return Err(waltr_script::Error::Invalid);
        }
        Ok(args)
    }
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Arguments {
    task: Option<String>,
    stream: Option<String>,
    outputs: Option<Vec<String>>,
    port: Option<String>,
    text: Option<String>,
    output: Option<String>,
    #[serde(skip)]
    helper: Option<super::authorized_tools::HelperArguments>,
    #[serde(skip)]
    helper_runner: Option<Arc<dyn crate::routine::ports::HelperRunner>>,
    #[serde(skip)]
    context_operation: Option<crate::routine::context::ContextOperation>,
    speech: Option<media::speech::SpeechSelector>,
    #[serde(skip)]
    backend: Option<Arc<dyn media::speech::SpeechBackend>>,
    field: Option<String>,
    name: Option<String>,
    cap: Option<usize>,
    snapshot: Option<bool>,
    utterances: Option<bool>,
    id: Option<String>,
    offset: Option<usize>,
    max: Option<usize>,
}
fn required(value: &Option<String>) -> Result<&str, waltr_script::Error> {
    value.as_deref().ok_or(waltr_script::Error::Invalid)
}
fn media_error(_: media::Error) -> waltr_script::Error {
    waltr_script::Error::Protocol
}

impl OperationPolicy for MediaHost {
    fn before(&self, request: &HostRequest, context: &HostContext) -> PortFuture<Value> {
        let checked = self
            .check(request, context)
            .map(|_| json!({"operation":request.operation}))
            .map_err(|_| PortError::Denied);
        Box::pin(async move { checked })
    }
    fn project_result(&self, _: &str, reply: &HostReply) -> Result<Option<Value>, PortError> {
        Ok(Some(match reply {
            HostReply::Handle(h) => json!({"kind":h.kind()}),
            HostReply::Bytes(b) => json!({"bytes":b.len()}),
            HostReply::Value(_) => json!({"status":"completed"}),
        }))
    }
}

impl MediaHost {
    async fn execute(
        self: Arc<Self>,
        request: HostRequest,
        context: HostContext,
        args: Arguments,
    ) -> Result<HostReply, waltr_script::Error> {
        let value = match request.operation.as_str() {
            "helper.submit" | "helper.idle" | "helper.seal" => {
                let Resource::Target {
                    target,
                    credit,
                    completed,
                } = self.resource(&request, &context)?
                else {
                    return Err(waltr_script::Error::Identity);
                };
                match request.operation.as_str() {
                    "helper.submit" => target
                        .submit(args.task.ok_or(waltr_script::Error::Invalid)?)
                        .await
                        .map_err(|_| waltr_script::Error::Protocol)?,
                    "helper.idle" => target
                        .idle()
                        .await
                        .map_err(|_| waltr_script::Error::Protocol)?,
                    _ => {
                        let result = target
                            .seal()
                            .await
                            .map_err(|_| waltr_script::Error::Protocol)?;
                        self.bindings
                            .driver
                            .record_helper(
                                result.session_id.as_str().into(),
                                result.definition_identity.clone(),
                            )
                            .await
                            .map_err(|_| waltr_script::Error::Cleanup)?;
                        completed.store(true, Ordering::Release);
                        return self.issue(
                            &context,
                            Resource::Helper {
                                result: Arc::new(result),
                                _credit: credit,
                            },
                        );
                    }
                }
                json!({"status":"accepted"})
            }
            "media.input.next" => {
                let capture = self
                    .bindings
                    .driver
                    .next_capture(required(&args.port)?.into())
                    .await
                    .map_err(|_| waltr_script::Error::Protocol)?;
                if let Some((id, reader)) = capture {
                    return self.issue(
                        &context,
                        Resource::Capture(Arc::new(Capture {
                            id,
                            format: reader.format(),
                            reader: Mutex::new(Some(reader)),
                            collected: AtomicBool::new(false),
                        })),
                    );
                }
                Value::Null
            }
            "media.input.acknowledge" => {
                let Resource::Capture(capture) = self.resource(&request, &context)? else {
                    return Err(waltr_script::Error::Identity);
                };
                if !capture.collected.load(Ordering::Acquire) {
                    return Err(waltr_script::Error::Protocol);
                }
                self.bindings
                    .driver
                    .acknowledge_capture(capture.id.clone())
                    .await
                    .map_err(|_| waltr_script::Error::Protocol)?;
                json!({"status":"completed"})
            }
            "media.input.seal" => {
                json!(
                    self.bindings
                        .driver
                        .seal_captures()
                        .await
                        .map_err(|_| waltr_script::Error::Protocol)?
                )
            }
            "media.text.open" => {
                let (writer, reader) = self
                    .bindings
                    .streams
                    .open(stream::Direction::Input, Format::Text, false)
                    .map_err(media_error)?;
                return self.issue(
                    &context,
                    Resource::TextFeed(Arc::new(TextFeed {
                        writer: tokio::sync::Mutex::new(Some((writer, 0))),
                        reader: Mutex::new(Some(reader)),
                    })),
                );
            }
            "media.text.write" | "media.text.close" => {
                let Resource::TextFeed(feed) = self.resource(&request, &context)? else {
                    return Err(waltr_script::Error::Identity);
                };
                let mut slot = feed
                    .writer
                    .try_lock()
                    .map_err(|_| waltr_script::Error::Protocol)?;
                if request.operation == "media.text.close" {
                    let (mut writer, _) = slot.take().ok_or(waltr_script::Error::Identity)?;
                    writer.close(writer.key()).await.map_err(media_error)?;
                } else {
                    let (writer, sequence) = slot.as_mut().ok_or(waltr_script::Error::Identity)?;
                    writer
                        .write(
                            writer.key(),
                            Chunk::Text(Text {
                                segment: sequence.to_string(),
                                text: args.text.ok_or(waltr_script::Error::Invalid)?,
                                replace: false,
                                committed: true,
                            }),
                        )
                        .await
                        .map_err(media_error)?;
                    *sequence += 1;
                }
                json!({"status":"completed"})
            }
            "media.synthesize" if request.handle.is_some() => {
                let Resource::TextFeed(feed) = self.resource(&request, &context)? else {
                    return Err(waltr_script::Error::Identity);
                };
                let input = feed
                    .reader
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .take()
                    .ok_or(waltr_script::Error::Identity)?;
                let output = self
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .outputs
                    .remove(required(&args.output)?)
                    .ok_or(waltr_script::Error::Identity)?;
                let backend = args.backend.as_ref().ok_or(waltr_script::Error::Invalid)?;
                let producer = if args.utterances == Some(true) {
                    backend.synthesize_utterances(input, output)
                } else {
                    backend.synthesize(input, output)
                }
                .map_err(media_error)?;
                producer.join().await.map_err(media_error)?;
                json!({"status":"completed"})
            }
            "helper.next" => {
                let value = self
                    .tool_streams
                    .next(required(&args.stream)?, self.cancel.clone())
                    .await
                    .map_err(|_| waltr_script::Error::Protocol)?;
                json!({"value":value})
            }
            name if name.starts_with("context.") => {
                let result = self
                    .bindings
                    .context
                    .as_ref()
                    .ok_or(waltr_script::Error::Denied)?
                    .execute(
                        args.context_operation.ok_or(waltr_script::Error::Invalid)?,
                        self.cancel.clone(),
                    )
                    .await
                    .map_err(|_| waltr_script::Error::Protocol)?;
                json!({"status":"completed","result":result})
            }
            "media.tee" => {
                let names = args
                    .outputs
                    .as_ref()
                    .filter(|names| !names.is_empty() && names.len() <= 4)
                    .ok_or(waltr_script::Error::Invalid)?;
                let (input, outputs) = {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| waltr_script::Error::Cleanup)?;
                    let input = state
                        .inputs
                        .remove(required(&args.port)?)
                        .ok_or(waltr_script::Error::Identity)?;
                    let outputs = names
                        .iter()
                        .map(|name| {
                            state
                                .outputs
                                .remove(name)
                                .ok_or(waltr_script::Error::Identity)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    (input, outputs)
                };
                stream::tee(input, outputs)
                    .map_err(media_error)?
                    .join()
                    .await
                    .map_err(media_error)?;
                json!({"status":"completed"})
            }
            "media.synthesize" if args.text.is_some() => {
                let text = args.text.as_deref().ok_or(waltr_script::Error::Invalid)?;
                let (writer, input) = self
                    .bindings
                    .streams
                    .open(stream::Direction::Input, Format::Text, false)
                    .map_err(media_error)?;
                let output = self
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .outputs
                    .remove(required(&args.output)?)
                    .ok_or(waltr_script::Error::Identity)?;
                let producer = args
                    .backend
                    .as_ref()
                    .ok_or(waltr_script::Error::Invalid)?
                    .synthesize(input, output)
                    .map_err(media_error)?;
                let (fed, joined) = tokio::join!(
                    async {
                        let result = write_text(writer, text).await;
                        if result.is_err() {
                            self.cancel.cancel();
                        }
                        result
                    },
                    async {
                        let result = producer.join().await;
                        if result.is_err() {
                            self.cancel.cancel();
                        }
                        result.map_err(media_error)
                    }
                );
                fed?;
                joined?;
                json!({"status":"completed"})
            }
            "media.transcribe" | "media.collect" | "media.synthesize" => {
                let input = if request.operation == "media.collect" && request.handle.is_some() {
                    let Resource::Capture(capture) = self.resource(&request, &context)? else {
                        return Err(waltr_script::Error::Identity);
                    };
                    let reader = capture
                        .reader
                        .lock()
                        .map_err(|_| waltr_script::Error::Cleanup)?
                        .take()
                        .ok_or(waltr_script::Error::Identity)?;
                    reader
                } else {
                    self.state
                        .lock()
                        .map_err(|_| waltr_script::Error::Cleanup)?
                        .inputs
                        .remove(required(&args.port)?)
                        .ok_or(waltr_script::Error::Identity)?
                };
                if request.operation == "media.collect" {
                    let cap = args
                        .cap
                        .filter(|cap| *cap > 0 && *cap <= 1048576)
                        .ok_or(waltr_script::Error::Limit)?;
                    let credit = self.charge(cap)?;
                    let text = if input.format() == Format::Text {
                        if args.snapshot == Some(true) {
                            return Err(waltr_script::Error::Invalid);
                        }
                        stream::collect(input, cap)
                            .await
                            .map_err(media_error)?
                            .segments()
                            .collect::<String>()
                    } else if args.snapshot == Some(true) {
                        let workspace = Workspace::new(
                            &input,
                            self.bindings.workspace_principal.clone(),
                            self.bindings.workspace_process.clone(),
                        );
                        let (producer, reader) = args
                            .backend
                            .as_ref()
                            .ok_or(waltr_script::Error::Invalid)?
                            .transcribe_snapshot(input, workspace)
                            .map_err(media_error)?;
                        let (joined, collected) = tokio::join!(
                            async {
                                let result = producer.join().await;
                                if result.is_err() {
                                    self.cancel.cancel();
                                }
                                result
                            },
                            async {
                                let result = reader.collect(cap).await;
                                if result.is_err() {
                                    self.cancel.cancel();
                                }
                                result
                            }
                        );
                        joined.map_err(media_error)?;
                        collected.map_err(media_error)?.chunks().collect::<String>()
                    } else {
                        let (output, reader) = self
                            .bindings
                            .streams
                            .open(stream::Direction::Output, Format::Text, false)
                            .map_err(media_error)?;
                        let producer = args
                            .backend
                            .as_ref()
                            .ok_or(waltr_script::Error::Invalid)?
                            .transcribe(input, output)
                            .map_err(media_error)?;
                        let (joined, collected) = tokio::join!(
                            async {
                                let result = producer.join().await;
                                if result.is_err() {
                                    self.cancel.cancel();
                                }
                                result
                            },
                            async {
                                let result = stream::collect(reader, cap).await;
                                if result.is_err() {
                                    self.cancel.cancel();
                                }
                                result
                            }
                        );
                        joined.map_err(media_error)?;
                        collected
                            .map_err(media_error)?
                            .segments()
                            .collect::<String>()
                    };
                    let reply = self.issue(
                        &context,
                        Resource::Text {
                            text: Arc::new(text),
                            helper_session: None,
                            _credit: credit,
                        },
                    )?;
                    if request.handle.is_some() {
                        let Resource::Capture(capture) = self.resource(&request, &context)? else {
                            return Err(waltr_script::Error::Identity);
                        };
                        capture.collected.store(true, Ordering::Release);
                    }
                    return Ok(reply);
                }
                let output = self
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .outputs
                    .remove(required(&args.output)?)
                    .ok_or(waltr_script::Error::Identity)?;
                let producer = if request.operation == "media.transcribe" {
                    args.backend
                        .as_ref()
                        .ok_or(waltr_script::Error::Invalid)?
                        .transcribe(input, output)
                } else {
                    args.backend
                        .as_ref()
                        .ok_or(waltr_script::Error::Invalid)?
                        .synthesize(input, output)
                }
                .map_err(media_error)?;
                producer.join().await.map_err(media_error)?;
                json!({"status":"completed"})
            }
            "media.finish" => {
                if !self.tool_streams.drained() {
                    return Err(waltr_script::Error::Protocol);
                }
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| waltr_script::Error::Cleanup)?;
                    if state.active != 1 {
                        return Err(waltr_script::Error::Protocol);
                    }
                    for (_, resource) in &state.resources {
                        if let Resource::Target { completed, .. } = resource {
                            if !completed.load(Ordering::Acquire) {
                                return Err(waltr_script::Error::Protocol);
                            }
                        }
                        if let Resource::TextFeed(feed) = resource {
                            if feed
                                .writer
                                .try_lock()
                                .map_err(|_| waltr_script::Error::Protocol)?
                                .is_some()
                                || feed
                                    .reader
                                    .lock()
                                    .map_err(|_| waltr_script::Error::Cleanup)?
                                    .is_some()
                            {
                                return Err(waltr_script::Error::Protocol);
                            }
                        }
                    }
                    if self
                        .definition
                        .envelope()
                        .spec
                        .ports
                        .iter()
                        .any(|(name, port)| {
                            port.required
                                && (state.inputs.contains_key(name)
                                    || state.outputs.contains_key(name)
                                    || state.uploads.contains_key(name)
                                    || state.binary_outputs.contains_key(name))
                        })
                    {
                        return Err(waltr_script::Error::Protocol);
                    }
                    state.finishing = true;
                }
                self.bindings
                    .driver
                    .finish()
                    .await
                    .map_err(|_| waltr_script::Error::Cleanup)?;
                self.state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .finished = true;
                json!({"status":"completed"})
            }
            "media.upload" => {
                let upload_owner = self
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .uploads
                    .remove(required(&args.port)?)
                    .ok_or(waltr_script::Error::Identity)?;
                let mut retained = upload_owner.lock().await;
                let upload = retained.as_mut().ok_or(waltr_script::Error::Identity)?;
                let owner = self
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .blob_owner
                    .clone()
                    .ok_or(waltr_script::Error::Identity)?;
                let result = async {
                    if let Some(committed) = self
                        .bindings
                        .driver
                        .committed_input(required(&args.port)?.into())
                        .await
                        .map_err(|_| waltr_script::Error::Protocol)?
                    {
                        let metadata = committed.metadata();
                        let kind_matches = match (upload.metadata.kind, metadata.image) {
                            (blob::BlobKind::File, None) => true,
                            (blob::BlobKind::Image(expected), Some(actual)) => expected == actual,
                            _ => false,
                        };
                        if metadata.display_name != upload.metadata.display_name
                            || metadata.content_type != upload.metadata.content_type
                            || upload.metadata.declared_bytes != Some(metadata.bytes)
                            || upload
                                .metadata
                                .expected_sha256
                                .is_some_and(|digest| digest != metadata.sha256)
                            || !kind_matches
                        {
                            return Err(waltr_script::Error::Identity);
                        }
                        return Ok(committed);
                    }
                    let handle = self
                        .bindings
                        .blobs
                        .begin(self.scope(), owner, upload.metadata.clone(), self.wait())
                        .await
                        .map_err(|_| waltr_script::Error::Invalid)?;
                    let mut sequence = 0u64;
                    while let Some(bytes) = upload
                        .source
                        .next(self.cancel.clone())
                        .await
                        .map_err(|_| waltr_script::Error::Protocol)?
                    {
                        if bytes.len() > 65536 {
                            return Err(waltr_script::Error::Limit);
                        }
                        self.bindings
                            .blobs
                            .append(self.scope(), handle.clone(), sequence, bytes, self.wait())
                            .await
                            .map_err(|_| waltr_script::Error::Invalid)?;
                        sequence = sequence.checked_add(1).ok_or(waltr_script::Error::Limit)?;
                    }
                    self.bindings
                        .blobs
                        .commit(self.scope(), handle, self.wait())
                        .await
                        .map_err(|_| waltr_script::Error::Invalid)
                }
                .await;
                if upload.source.close().await.is_err() {
                    self.state
                        .lock()
                        .map_err(|_| waltr_script::Error::Cleanup)?
                        .cleanup_failed = true;
                    return Err(waltr_script::Error::Cleanup);
                }
                retained.take();
                return self.issue(&context, Resource::Blob(result?));
            }
            "helper.run" | "helper.open" => {
                let resource = request
                    .handle
                    .as_ref()
                    .map(|_| self.resource(&request, &context))
                    .transpose()?;
                let profile = args.helper.ok_or(waltr_script::Error::Invalid)?;
                let helpers = args
                    .helper_runner
                    .unwrap_or_else(|| self.bindings.helpers.clone());
                let mut capabilities = if resource.is_some() {
                    helpers
                        .input_capabilities(profile.selection.clone())
                        .await
                        .map_err(|_| waltr_script::Error::Denied)?
                } else {
                    TransientModelCapabilities {
                        png: false,
                        jpeg: false,
                        webp: false,
                        pdf: false,
                        text: true,
                        max_body_bytes: 65536,
                    }
                };
                capabilities.text &= profile.input.text;
                if resource.is_some() {
                    helpers
                        .preflight_payload(
                            profile.selection.clone(),
                            capabilities,
                            profile.capture.clone(),
                        )
                        .await
                        .map_err(|_| waltr_script::Error::Denied)?;
                }
                let credit = self.charge(profile.completion_bytes)?;
                let attachment = match &resource {
                    Some(Resource::Blob(blob)) => {
                        if blob.metadata().bytes > profile.input.max_bytes as u64 {
                            return Err(waltr_script::Error::Limit);
                        }
                        let kind = match (
                            &blob.metadata().image,
                            blob.metadata().content_type.as_str(),
                        ) {
                            (Some(format), _) => match format {
                                blob::ImageFormat::Png => TransientInputKind::Png,
                                blob::ImageFormat::Jpeg => TransientInputKind::Jpeg,
                                blob::ImageFormat::WebP => TransientInputKind::WebP,
                            },
                            (None, "application/pdf") => TransientInputKind::Pdf,
                            (None, "text/plain") => TransientInputKind::Utf8,
                            _ => return Err(waltr_script::Error::Denied),
                        };
                        if !supported(capabilities, kind) {
                            return Err(waltr_script::Error::Denied);
                        }
                        let mut bytes = self
                            .bindings
                            .attachments
                            .attachment_buffer(kind, blob.metadata().bytes as usize)
                            .map_err(|_| waltr_script::Error::Limit)?;
                        #[cfg(test)]
                        BLOB_READER_OPENS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let reader = self
                            .bindings
                            .blobs
                            .reader(self.scope(), blob.clone(), self.wait())
                            .await
                            .map_err(|_| waltr_script::Error::Identity)?;
                        let read = async {
                            loop {
                                let chunk = self
                                    .bindings
                                    .blobs
                                    .read(
                                        blob::ReadAccess::Invocation(self.scope()),
                                        reader.clone(),
                                        self.wait(),
                                    )
                                    .await
                                    .map_err(|_| waltr_script::Error::Identity)?;
                                bytes
                                    .append(&chunk.bytes)
                                    .map_err(|_| waltr_script::Error::Limit)?;
                                if chunk.eof {
                                    break;
                                }
                            }
                            Ok(())
                        }
                        .await;
                        reader.close();
                        read?;
                        Some(bytes.finish().map_err(|_| waltr_script::Error::Limit)?)
                    }
                    Some(resource) => {
                        if !capabilities.text {
                            return Err(waltr_script::Error::Denied);
                        }
                        let text = resource_text(resource, profile.field.as_deref())?;
                        if text.len() > profile.input.max_bytes {
                            return Err(waltr_script::Error::Limit);
                        }
                        Some(
                            self.bindings
                                .attachments
                                .attachment(TransientInputKind::Utf8, text.as_bytes().to_vec())
                                .map_err(|_| waltr_script::Error::Limit)?,
                        )
                    }
                    None => None,
                };
                let observer = request.callback.map(|callback| {
                    Arc::new(ProposalObserver {
                        callback,
                        state: self.state.clone(),
                        context: context.clone(),
                        limits: self.definition.limits().clone(),
                    }) as Arc<dyn CompletionObserver>
                });
                if request.operation == "helper.open" {
                    let target = helpers
                        .open(
                            HelperRequest {
                                selection: profile.selection,
                                task: profile.task,
                                capture: profile.capture,
                                max_turns: profile.max_turns,
                                deadline: Duration::from_millis(profile.deadline_ms),
                                observer,
                            },
                            HelperPayload {
                                budget: self.bindings.attachments.clone(),
                                attachments: Vec::new(),
                                capabilities,
                                completion_bytes: profile.completion_bytes,
                                response_bytes: profile.response_bytes,
                            },
                            self.cancel.clone(),
                        )
                        .await
                        .map_err(|_| waltr_script::Error::Protocol)?;
                    return self.issue(
                        &context,
                        Resource::Target {
                            target,
                            credit,
                            completed: Arc::new(AtomicBool::new(false)),
                        },
                    );
                }
                let result = helpers
                    .run_with_inputs(
                        HelperRequest {
                            selection: profile.selection.clone(),
                            task: profile.task.clone(),
                            capture: profile.capture.clone(),
                            max_turns: profile.max_turns,
                            deadline: Duration::from_millis(profile.deadline_ms),
                            observer,
                        },
                        profile
                            .source
                            .as_ref()
                            .map(|source| {
                                self.bindings
                                    .context
                                    .as_ref()
                                    .ok_or(waltr_script::Error::Denied)?
                                    .resolve_source(source)
                                    .map_err(|_| waltr_script::Error::Denied)
                            })
                            .transpose()?,
                        Some(HelperPayload {
                            budget: self.bindings.attachments.clone(),
                            attachments: attachment.into_iter().collect(),
                            capabilities,
                            completion_bytes: profile.completion_bytes,
                            response_bytes: profile.response_bytes,
                        }),
                        self.cancel.clone(),
                    )
                    .await
                    .map_err(|_| waltr_script::Error::Protocol)?;
                self.bindings
                    .driver
                    .record_helper(
                        result.session_id.as_str().to_owned(),
                        result.definition_identity.clone(),
                    )
                    .await
                    .map_err(|_| waltr_script::Error::Cleanup)?;
                return self.issue(
                    &context,
                    Resource::Helper {
                        result: Arc::new(result),
                        _credit: credit,
                    },
                );
            }
            "media.select" => {
                let resource = self.resource(&request, &context)?;
                let Resource::Helper { result, .. } = resource else {
                    return Err(waltr_script::Error::Invalid);
                };
                let capture = result.captures.last().ok_or(waltr_script::Error::Invalid)?;
                let value = match &args.field {
                    Some(field) => capture
                        .value
                        .pointer(field)
                        .ok_or(waltr_script::Error::Invalid)?,
                    None => &capture.value,
                };
                let text = match value {
                    Value::String(text) => text.clone(),
                    value => {
                        serde_json::to_string(value).map_err(|_| waltr_script::Error::Value)?
                    }
                };
                if text.len() > 1048576 {
                    return Err(waltr_script::Error::Limit);
                }
                let credit = self.charge(text.len())?;
                return self.issue(
                    &context,
                    Resource::Text {
                        text: Arc::new(text),
                        helper_session: Some(result.session_id.as_str().to_owned()),
                        _credit: credit,
                    },
                );
            }
            "media.emit" | "media.publish" | "media.read" | "media.readText" => {
                let resource = self.resource(&request, &context)?;
                let text = resource_text(&resource, args.field.as_deref())?;
                if request.operation == "media.readText" {
                    return read_text_value(text, args.offset, args.max, self.definition.limits());
                }
                if request.operation == "media.read" {
                    return read_text(text, args.offset, args.max);
                }
                if text.len() > 1048576 {
                    return Err(waltr_script::Error::Limit);
                }
                if request.operation == "media.emit" {
                    let output = self
                        .state
                        .lock()
                        .map_err(|_| waltr_script::Error::Cleanup)?
                        .outputs
                        .remove(required(&args.output)?)
                        .ok_or(waltr_script::Error::Identity)?;
                    write_text(output, text).await?;
                    json!({"status":"completed"})
                } else {
                    let scope = InvocationTextScope::new(
                        self.binding.principal().into(),
                        self.binding.id().into(),
                    )
                    .map_err(|_| waltr_script::Error::Identity)?;
                    let helper = match &resource {
                        Resource::Text { helper_session, .. } => helper_session.clone(),
                        Resource::Helper { result, .. } => {
                            Some(result.session_id.as_str().to_owned())
                        }
                        _ => None,
                    };
                    let source = ResourceSource {
                        resource,
                        field: args.field,
                        offset: 0,
                    };
                    let result = self
                        .bindings
                        .text
                        .publish(
                            scope,
                            required(&args.name)?.into(),
                            helper,
                            Box::new(source),
                            self.cancel.clone(),
                        )
                        .await
                        .map_err(|_| waltr_script::Error::Cleanup)?;
                    serde_json::to_value(result).map_err(|_| waltr_script::Error::Value)?
                }
            }
            "media.lease" => {
                let Resource::Blob(blob) = self.resource(&request, &context)? else {
                    return Err(waltr_script::Error::Identity);
                };
                let name = required(&args.output)?;
                let port = self
                    .definition
                    .envelope()
                    .spec
                    .ports
                    .get(name)
                    .ok_or(waltr_script::Error::Identity)?;
                if blob.metadata().bytes > u64::from(port.max_bytes)
                    || (matches!(port.media, Media::Image) && blob.metadata().image.is_none())
                {
                    return Err(waltr_script::Error::Limit);
                }
                if self
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .binary_outputs
                    .remove(name)
                    .is_none()
                {
                    return Err(waltr_script::Error::Identity);
                }
                if self
                    .bindings
                    .leases
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .len()
                    >= 32
                {
                    return Err(waltr_script::Error::Limit);
                }
                let lease = self
                    .bindings
                    .blobs
                    .transfer_output(self.scope(), blob, self.wait())
                    .await
                    .map_err(|_| waltr_script::Error::Cleanup)?;
                let result =
                    json!({"bytes":lease.metadata().bytes,"expiresAtMs":lease.expires_at_ms()});
                self.bindings
                    .driver
                    .record_binary(name.to_owned(), lease.clone())
                    .await
                    .map_err(|_| waltr_script::Error::Cleanup)?;
                self.bindings
                    .leases
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .push(lease);
                result
            }
            "media.proposal.read" => {
                let proposal = self
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .proposals
                    .get(required(&args.id)?)
                    .cloned()
                    .ok_or(waltr_script::Error::Identity)?;
                match args.field.as_deref() {
                    Some("response") => {
                        return read_text(&proposal.response, args.offset, args.max);
                    }
                    Some("evidence") => {
                        let text = serde_json::to_string(&proposal.evidence)
                            .map_err(|_| waltr_script::Error::Value)?;
                        return read_text(&text, args.offset, args.max);
                    }
                    _ => return Err(waltr_script::Error::Invalid),
                }
            }
            _ => return Err(waltr_script::Error::Denied),
        };
        Ok(HostReply::Value(StructuredValue::new(
            value,
            self.definition.limits(),
        )?))
    }
}
fn supported(c: TransientModelCapabilities, kind: TransientInputKind) -> bool {
    match kind {
        TransientInputKind::Png => c.png,
        TransientInputKind::Jpeg => c.jpeg,
        TransientInputKind::WebP => c.webp,
        TransientInputKind::Pdf => c.pdf,
        TransientInputKind::Utf8 => c.text,
    }
}
async fn write_text(mut output: Writer, mut text: &str) -> Result<(), waltr_script::Error> {
    let mut index = 0usize;
    while !text.is_empty() {
        let mut end = text.len().min(60000);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        output
            .write(
                output.key(),
                Chunk::Text(Text {
                    segment: format!("result-{index}"),
                    text: text[..end].into(),
                    replace: false,
                    committed: true,
                }),
            )
            .await
            .map_err(media_error)?;
        index += 1;
        text = &text[end..];
    }
    output.close(output.key()).await.map_err(media_error)
}

fn resource_text<'a>(
    resource: &'a Resource,
    field: Option<&str>,
) -> Result<&'a str, waltr_script::Error> {
    match (resource, field) {
        (Resource::Text { text, .. }, None | Some("response")) => Ok(text.as_str()),
        (Resource::Helper { result, .. }, None | Some("response")) => Ok(&result.response),
        (Resource::Helper { result, .. }, Some(field)) => result
            .captures
            .last()
            .and_then(|c| c.value.get(field))
            .and_then(Value::as_str)
            .ok_or(waltr_script::Error::Invalid),
        _ => Err(waltr_script::Error::Identity),
    }
}
fn read_text_value(
    text: &str,
    offset: Option<usize>,
    max: Option<usize>,
    limits: &waltr_script::Limits,
) -> Result<HostReply, waltr_script::Error> {
    let max = max.unwrap_or(32768);
    if offset.is_some() || max == 0 || max > 65536 {
        return Err(waltr_script::Error::Invalid);
    }
    if text.len() > max {
        return Err(waltr_script::Error::Limit);
    }
    Ok(HostReply::Value(StructuredValue::new(json!(text), limits)?))
}
fn read_text(
    text: &str,
    offset: Option<usize>,
    max: Option<usize>,
) -> Result<HostReply, waltr_script::Error> {
    let offset = offset.unwrap_or(0);
    let max = max.unwrap_or(32768);
    if max == 0 || max > 65536 || offset > text.len() {
        return Err(waltr_script::Error::Invalid);
    }
    Ok(HostReply::Bytes(
        text.as_bytes()[offset..text.len().min(offset.saturating_add(max))].to_vec(),
    ))
}
struct ResourceSource {
    resource: Resource,
    field: Option<String>,
    offset: usize,
}
impl TextSource for ResourceSource {
    fn read<'a>(&'a mut self, destination: &'a mut [u8]) -> TextSourceFuture<'a> {
        Box::pin(async move {
            let text = resource_text(&self.resource, self.field.as_deref())
                .map_err(|_| crate::routine::text_results::TextResultError::Invalid)?;
            let end = text.len().min(self.offset + destination.len());
            let bytes = end - self.offset;
            destination[..bytes].copy_from_slice(&text.as_bytes()[self.offset..end]);
            self.offset = end;
            Ok(bytes)
        })
    }
}
struct ProposalObserver {
    callback: waltr_script::CallbackRef,
    state: Arc<Mutex<State>>,
    context: HostContext,
    limits: Limits,
}
impl CompletionObserver for ProposalObserver {
    fn observe(
        &self,
        proposal: CompletionProposal,
        cancellation: CancellationToken,
    ) -> CompletionObserverFuture {
        let callback = self.callback.clone();
        let state = self.state.clone();
        let context = self.context.clone();
        let limits = self.limits.clone();
        Box::pin(async move {
            let id = uuid::Uuid::new_v4().to_string();
            let value = json!({"sessionId":proposal.session_id,"proposalId":proposal.proposal_id,"turn":proposal.turn,"responseBytes":proposal.response.len(),"evidenceCount":proposal.evidence.len(),"resourceRef":id});
            {
                let mut state = state.lock().map_err(|_| CompletionObserverError::Failed)?;
                if state.engine != Some(context.invocation()) || state.proposals.len() >= 16 {
                    return Err(CompletionObserverError::Failed);
                }
                state.proposals.insert(id.clone(), Arc::new(proposal));
            }
            let verdict = tokio::select! { biased;
                () = cancellation.cancelled() => Err(CompletionObserverError::Failed),
                verdict = callback.invoke(StructuredValue::new(value, &limits).map_err(|_| CompletionObserverError::Invalid)?) => verdict.map_err(|_| CompletionObserverError::Failed),
            };
            state
                .lock()
                .map_err(|_| CompletionObserverError::Failed)?
                .proposals
                .remove(&id);
            serde_json::from_value::<CompletionDecision>(verdict?.into_json())
                .map_err(|_| CompletionObserverError::Invalid)
        })
    }
}

impl HostDispatcher for MediaHost {
    fn dispatch(&self, request: HostRequest, context: HostContext) -> HostFuture {
        let this = Arc::new(self.clone());
        Box::pin(async move {
            let args = this.check(&request, &context)?;
            let mut jobs = this.jobs.lock().await;
            if jobs.len() >= this.definition.limits().host_calls {
                return Err(waltr_script::Error::Limit);
            }
            {
                let mut state = this
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?;
                if state.finishing || state.finished || state.cleaned || this.cancel.is_cancelled()
                {
                    return Err(waltr_script::Error::Denied);
                }
                state.active += 1;
            }
            let worker = this.clone();
            let job = Arc::new(tokio::sync::Mutex::new(Some(tokio::spawn(async move {
                let result = async {
                    let mut started = worker.startup.lock().await;
                    if !*started {
                        worker
                            .bindings
                            .driver
                            .start()
                            .await
                            .map_err(|_| waltr_script::Error::Cleanup)?;
                        *started = true;
                    }
                    drop(started);
                    worker.clone().execute(request, context, args).await
                }
                .await;
                if matches!(result, Err(waltr_script::Error::Cleanup)) {
                    worker
                        .state
                        .lock()
                        .map_err(|_| waltr_script::Error::Cleanup)?
                        .cleanup_failed = true;
                }
                worker
                    .state
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .active -= 1;
                result
            }))));
            jobs.push(job.clone());
            drop(jobs);
            let mut task = job.lock().await;
            let result = task
                .as_mut()
                .ok_or(waltr_script::Error::Cleanup)?
                .await
                .map_err(|_| waltr_script::Error::Panic)?;
            task.take();
            result
        })
    }
    fn cleanup(&self, _: InvocationId) -> CleanupFuture {
        let this = self.clone();
        Box::pin(async move { this.close().await.map_err(|_| waltr_script::Error::Cleanup) })
    }
}
impl MediaHost {
    async fn close(&self) -> Result<(), PortError> {
        let mut cleanup = self.cleanup.lock().await;
        if let Some(result) = &cleanup.result {
            return *result;
        }
        if cleanup.task.is_none() {
            let this = self.clone();
            cleanup.task = Some(self.runtime.spawn(async move { this.close_inner().await }));
        }
        let result = cleanup
            .task
            .as_mut()
            .ok_or(PortError::Uncertain)?
            .await
            .unwrap_or(Err(PortError::Uncertain));
        cleanup.task.take();
        cleanup.result = Some(result);
        result
    }
    async fn close_inner(&self) -> Result<(), PortError> {
        self.tool_streams.cancel();
        self.cancel.cancel();
        self.bindings.driver.cancel();
        let jobs = self.jobs.lock().await.clone();
        let mut clean = true;
        for job in jobs {
            let mut task = job.lock().await;
            if let Some(task) = task.as_mut() {
                clean &= !matches!(task.await, Err(_) | Ok(Err(waltr_script::Error::Cleanup)));
            }
            task.take();
        }
        clean &= self.bindings.helpers.cleanup().await.is_ok();
        if let Some(context) = &self.bindings.context {
            clean &= context.cleanup().await.is_ok();
        }
        clean &= self.bindings.driver.join().await.is_ok();
        let (uploads, owner) = {
            let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
            state.inputs.clear();
            state.outputs.clear();
            state.resources.clear();
            state.proposals.clear();
            (state.upload_owners.clone(), state.blob_owner.clone())
        };
        for owner in uploads {
            let mut retained = owner.lock().await;
            if let Some(upload) = retained.as_mut() {
                if upload.source.close().await.is_ok() {
                    retained.take();
                } else {
                    clean = false;
                }
            }
        }
        if let Some(owner) = owner {
            clean &= self
                .bindings
                .blobs
                .close_invocation(
                    self.scope(),
                    owner,
                    blob::Wait {
                        cancellation: CancellationToken::new(),
                        deadline: tokio::time::Instant::now() + Duration::from_secs(30),
                    },
                )
                .await
                .is_ok();
        }
        let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
        state.cleanup_failed |= !clean;
        clean &= !state.cleanup_failed;
        state.cleaned = clean;
        if clean {
            state.blob_owner.take();
        }
        if clean {
            Ok(())
        } else {
            Err(PortError::Uncertain)
        }
    }
}
struct MediaReservation(Arc<MediaHost>);
impl Reservation for MediaReservation {
    fn prepare(&self) -> PortFuture<()> {
        let host = self.0.clone();
        Box::pin(async move {
            let owner = host
                .bindings
                .blobs
                .open_invocation(host.scope(), host.wait())
                .await
                .map_err(|_| PortError::Unavailable)?;
            let mut state = host.state.lock().map_err(|_| PortError::Uncertain)?;
            state.blob_owner = Some(owner.clone());
            state.prepared = true;
            drop(state);
            host.bindings
                .driver
                .bind_blobs(host.binding.clone(), owner)?;
            Ok(())
        })
    }
    fn abort(&self) -> PortFuture<()> {
        let host = self.0.clone();
        Box::pin(async move { host.close().await })
    }
    fn finalize(&self, interrupted: bool) -> PortFuture<()> {
        let result = self
            .0
            .state
            .lock()
            .map_err(|_| PortError::Uncertain)
            .and_then(|s| {
                if s.cleaned && (interrupted || s.finished) {
                    Ok(())
                } else {
                    Err(PortError::Uncertain)
                }
            });
        Box::pin(async move { result })
    }
    fn validate_success(&self) -> Result<(), PortError> {
        if self.0.tool_streams.was_drained() {
            Ok(())
        } else {
            Err(PortError::Invalid)
        }
    }
}
