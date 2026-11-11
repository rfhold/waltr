//! Explicit bounded native output capability. The app owns devices and file destinations.
//!
//! A finite selection uses an already authorized local handle:
//! ```no_run
//! # async fn finite<T: waltr_client::RoutineInteractionTransport>(transport: &T, invocation: &waltr_desktop::interaction::Invocation, source: Box<dyn waltr_desktop::media::Source>, bytes: u64, guard: &waltr_desktop::typing::Guard<'_>, consumer: &dyn waltr_desktop::interaction::Consumer) -> Result<(), waltr_desktop::host::Error> {
//! use waltr_client::interaction::wire;
//! use waltr_desktop::{host::Observer, media::{self, BinaryInput}};
//! let mut inputs = [BinaryInput {
//!     port: "document".into(),
//!     declaration: wire::BinaryInputDeclaration {
//!         display_name: "selected.pdf".into(), content_type: "application/pdf".into(),
//!         declared_bytes: Some(bytes), expected_sha256: None,
//!         purpose: Some(wire::binary_input_declaration::Purpose::File(wire::FilePurpose {})),
//!     }, source,
//! }];
//! let output = media::run(transport, invocation, &mut inputs, Some("text"), guard, &Observer::default(), consumer).await?;
//! // Recheck the guard immediately before any subsequent app-owned effect.
//! guard.check().map_err(|_| waltr_desktop::host::Error::Cancelled)?;
//! # drop(output); Ok(()) }
//! ```
use crate::{
    host::Error,
    typing::{Guard, LocalFuture},
};
use waltr_client::{
    RoutineBlobDescriptor, RoutinePort,
    interaction::{BinaryOutputGrant, InteractionEvent},
};

/// Maximum file input size, independent of helper-attachment budgets.
pub const FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum encoded image input size; server quarantine validates the actual image.
pub const IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Maximum local read and upload chunk, further narrowed by the server.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// A trusted, user-selected finite handle, not a server path or URL.
/// Read futures must be cancellation-safe; retain any worker outside them for `finish`.
pub trait Source {
    /// Return at most `max_bytes`; empty means EOF. Never prefetch an unbounded file.
    fn read(&mut self, max_bytes: usize) -> LocalFuture<'_, Result<Vec<u8>, Error>>;
    /// Close the handle, stop producers, and join workers even after a cancelled read.
    /// This future is always awaited and must not be cancelled by the host.
    fn finish(&mut self, success: bool) -> LocalFuture<'_, Result<(), Error>>;
}

/// One explicit finite input. Metadata is declared before admission; bytes stay typed.
pub struct BinaryInput {
    /// Exact input port from the pinned descriptor.
    pub port: String,
    /// Finite name, media type, size, and file/image purpose. No local path is sent.
    pub declaration: waltr_client::interaction::wire::BinaryInputDeclaration,
    /// Already authorized local content; the app owns picker and path policy.
    pub source: Box<dyn Source>,
}
impl std::fmt::Debug for BinaryInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinaryInput").finish_non_exhaustive()
    }
}

pub(crate) struct Producer<'a> {
    inputs: &'a mut [BinaryInput],
    uploads: Vec<Option<waltr_client::interaction::BlobUpload>>,
}
impl<'a> Producer<'a> {
    pub(crate) fn new(inputs: &'a mut [BinaryInput]) -> Self {
        Self {
            uploads: (0..inputs.len()).map(|_| None).collect(),
            inputs,
        }
    }
    pub(crate) fn declarations(
        &self,
        invocation: &crate::interaction::Invocation,
    ) -> Result<Vec<waltr_client::interaction::wire::InputDeclaration>, Error> {
        use waltr_client::{RoutineDirection, RoutineMedia, interaction::wire as w};
        let mut declarations = Vec::new();
        for input in self.inputs.iter() {
            let port = invocation
                .limits
                .definition
                .ports
                .iter()
                .find(|port| port.name == input.port && port.direction == RoutineDirection::Input)
                .ok_or(Error::Unsupported)?;
            let size = input.declaration.declared_bytes.ok_or(Error::Unsupported)?;
            let limit = match (&input.declaration.purpose, port.media) {
                (Some(w::binary_input_declaration::Purpose::File(_)), RoutineMedia::File) => {
                    FILE_BYTES.min(invocation.limits.server.file_bytes)
                }
                (Some(w::binary_input_declaration::Purpose::Image(_)), RoutineMedia::Image)
                    if size != 0 =>
                {
                    IMAGE_BYTES.min(invocation.limits.server.image_bytes)
                }
                _ => return Err(Error::Unsupported),
            };
            if size > limit
                || size > port.max_bytes.0
                || declarations
                    .iter()
                    .any(|d: &w::InputDeclaration| d.port == input.port)
            {
                return Err(Error::Unsupported);
            }
            declarations.push(w::InputDeclaration {
                port: input.port.clone(),
                format: Some(w::input_declaration::Format::Binary(
                    input.declaration.clone(),
                )),
            });
        }
        Ok(declarations)
    }
    pub(crate) async fn send(
        &mut self,
        input: &mut waltr_client::interaction::InteractionInputHandle,
        transfers: &waltr_client::interaction::MediaTransfers,
        guard: &Guard<'_>,
    ) -> Result<(), Error> {
        for (source, owner) in self.inputs.iter_mut().zip(&mut self.uploads) {
            guard.check().map_err(|_| Error::Cancelled)?;
            *owner = Some(
                input
                    .begin_upload(transfers, &source.port)
                    .map_err(|_| Error::Interaction)?,
            );
            let upload = owner.as_mut().ok_or(Error::Interaction)?;
            upload.begin().await.map_err(|_| Error::Interaction)?;
            let chunk = (upload.chunk_bytes() as usize).min(CHUNK_BYTES);
            if chunk == 0 {
                return Err(Error::Interaction);
            }
            let mut remaining = source
                .declaration
                .declared_bytes
                .ok_or(Error::Unsupported)?;
            while remaining != 0 {
                let max = chunk.min(usize::try_from(remaining).unwrap_or(chunk));
                let bytes = source.source.read(max).await?;
                guard.check().map_err(|_| Error::Cancelled)?;
                if bytes.is_empty() || bytes.len() > max {
                    return Err(Error::Interaction);
                }
                upload
                    .append(&bytes)
                    .await
                    .map_err(|_| Error::Interaction)?;
                remaining -= bytes.len() as u64;
            }
            // A changed or dishonest source cannot silently truncate the selected content.
            if !source.source.read(1).await?.is_empty() {
                return Err(Error::Interaction);
            }
            guard.check().map_err(|_| Error::Cancelled)?;
            let (_, event) = upload.commit().await.map_err(|_| Error::Interaction)?;
            input
                .send(event.event.ok_or(Error::Interaction)?)
                .await
                .map_err(|_| Error::Interaction)?;
        }
        Ok(())
    }
    pub(crate) async fn finish(&mut self, success: bool) -> Result<(), Error> {
        let mut result = Ok(());
        for upload in self.uploads.iter_mut().flatten() {
            if !success
                && !matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(30), upload.cancel()).await,
                    Ok(Ok(()))
                )
            {
                result = Err(Error::Interaction);
            }
        }
        for input in self.inputs.iter_mut() {
            if let Err(error) = input.source.finish(success).await {
                result = Err(error);
            }
        }
        result
    }
}

/// Successful finite interaction, returned only after local and remote cleanup.
pub struct Output {
    /// Accepted invocation identity.
    pub invocation: String,
    /// Selected final text, if requested. No command or typing fallback is selected here.
    pub text: Option<String>,
}
impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output").finish_non_exhaustive()
    }
}

/// Run finite file/image inputs or a descriptor with no input. No device is activated.
/// Polls output concurrently with upload, awaits bounded writes, and never retries uncertain RPCs.
/// Retain and await this future after cancellation; dropping it is not cleanup proof.
/// # Errors
/// Rejects unsupported selections, size changes, failed uploads, early EOF, or invalidation.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::future_not_send
)]
pub async fn run<T: waltr_client::RoutineInteractionTransport>(
    transport: &T,
    invocation: &crate::interaction::Invocation,
    inputs: &mut [BinaryInput],
    text_output: Option<&str>,
    guard: &Guard<'_>,
    observer: &crate::host::Observer,
    consumer: &dyn crate::interaction::Consumer,
) -> Result<Output, Error> {
    use crate::host::{Diagnostic, RpcFailure};
    use futures_util::StreamExt;
    use waltr_client::{
        RoutineDirection, RoutineMedia,
        interaction::{InteractionSession, wire as w},
    };
    let timeout_error = || {
        observer.emit(Diagnostic::RpcFailure(RpcFailure::AcceptanceTimeout));
        Error::Interaction
    };
    let mut producer = Producer::new(inputs);
    let mut session_owner: Option<InteractionSession> = None;
    let result = {
        let operation = async {
            guard.check().map_err(|_| Error::Cancelled)?;
            let declarations = producer.declarations(invocation)?;
            for port in &invocation.limits.definition.ports {
                if port.direction == RoutineDirection::Output
                    && port.media != RoutineMedia::Text
                    && !consumer.sink().is_some_and(|sink| sink.supports(port))
                {
                    return Err(Error::Unsupported);
                }
            }
            if text_output.is_some_and(|name| {
                !invocation.limits.definition.ports.iter().any(|p| {
                    p.name == name
                        && p.direction == RoutineDirection::Output
                        && p.media == RoutineMedia::Text
                })
            }) {
                return Err(Error::Unsupported);
            }
            let open = w::InteractionOpen {
                selection: Some(invocation.selection.clone()),
                inputs: declarations,
            };
            invocation
                .limits
                .validate_open(&open)
                .map_err(|_| Error::Unsupported)?;
            let deadline = tokio::time::Instant::now() + crate::interaction::ACCEPTANCE_TIMEOUT;
            let session = tokio::time::timeout_at(
                deadline,
                transport.interact(open, invocation.limits.clone()),
            )
            .await
            .map_err(|_| timeout_error())?
            .map_err(|_| Error::Interaction)?;
            session_owner = Some(session);
            let session = session_owner.as_mut().ok_or(Error::Interaction)?;
            let (accepted, ready) = tokio::sync::oneshot::channel();
            let finished = std::cell::Cell::new(false);
            let receive = async {
                let mut accepted = Some(accepted);
                let mut id = None;
                while let Some(event) = session.output.next().await {
                    let event = event.map_err(|_| Error::Interaction)?;
                    guard.check().map_err(|_| Error::Cancelled)?;
                    if let InteractionEvent::Accepted { invocation_id } = &event {
                        id = Some(invocation_id.clone());
                        if let Some(tx) = accepted.take() {
                            let _sent = tx.send(());
                        }
                        consumer.progress(crate::interaction::Progress::Accepted);
                    }
                    if let Some(sink) = consumer.sink() {
                        let frame = match &event {
                            InteractionEvent::Blob { port, descriptor } => Frame::Blob {
                                port,
                                descriptor,
                                grant: session
                                    .output
                                    .take_blob_grant(port)
                                    .ok_or(Error::Interaction)?,
                            },
                            _ => Frame::Stream(&event),
                        };
                        sink.write(frame, guard).await?;
                    }
                    if let Some(port) = text_output {
                        consumer.output(&session.output, port, None, guard).await?;
                    }
                }
                if !finished.get()
                    || !session.output.cleanup_verified()
                    || session
                        .output
                        .terminal()
                        .is_none_or(|t| t.status != w::InvocationStatus::Succeeded as i32)
                {
                    return Err(Error::Interaction);
                }
                let text = text_output
                    .map(|port| {
                        session
                            .output
                            .final_text(port)
                            .map_err(|_| Error::Interaction)
                    })
                    .transpose()?;
                if let (Some(port), Some(text)) = (text_output, text.as_deref()) {
                    guard.check().map_err(|_| Error::Cancelled)?;
                    consumer
                        .output(&session.output, port, Some(text), guard)
                        .await?;
                }
                Ok::<_, Error>(Output {
                    invocation: id.ok_or(Error::Interaction)?,
                    text,
                })
            };
            let send = async {
                tokio::time::timeout_at(deadline, ready)
                    .await
                    .map_err(|_| timeout_error())?
                    .map_err(|_| Error::Interaction)?;
                producer
                    .send(&mut session.input, &transport.media_transfers(), guard)
                    .await?;
                guard.check().map_err(|_| Error::Cancelled)?;
                session
                    .input
                    .finish_input()
                    .await
                    .map_err(|_| Error::Interaction)?;
                finished.set(true);
                consumer.progress(crate::interaction::Progress::InputClosed);
                Ok::<_, Error>(())
            };
            let (output, ()) = tokio::try_join!(receive, send)?;
            Ok(output)
        };
        tokio::pin!(operation);
        loop {
            tokio::select! { biased;
                () = guard.cancel.cancelled() => break Err(Error::Cancelled),
                () = guard.generation.cancelled() => break Err(Error::Cancelled),
                result = &mut operation => break result,
                () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                    if guard.check().is_err() { break Err(Error::Cancelled); }
                }
            }
        }
    };
    if result.is_err() {
        if let Some(session) = &session_owner {
            session.input.cancel();
        }
    }
    let local = producer.finish(result.is_ok()).await;
    observer.emit(Diagnostic::SourceCleanup(local));
    if let Some(session) = &mut session_owner {
        let proof = if result.is_err() || local.is_err() {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                session.output.cancel_and_join(),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
        } else {
            session.output.cleanup_verified()
        };
        observer.emit(Diagnostic::RemoteCleanup(proof));
    }
    let sink = match consumer.sink() {
        Some(sink) => {
            sink.finish(
                result.is_ok() && local.is_ok() && guard.check().is_ok(),
                guard,
            )
            .await
        }
        None => Ok(()),
    };
    let output = result?;
    local?;
    sink?;
    guard.check().map_err(|_| Error::Cancelled)?;
    Ok(output)
}

/// Shared-client validated output. There are no server paths or URL-fetch instructions.
pub enum Frame<'a> {
    /// Includes exact PCM format, typed byte chunks, sequence, and close events.
    Stream(&'a InteractionEvent),
    /// Single-use authority for `MediaTransfers::read_output`; not an arbitrary blob id.
    Blob {
        /// Selected output port.
        port: &'a str,
        /// Validated finite metadata, not reader authority.
        descriptor: &'a RoutineBlobDescriptor,
        /// Sensitive single-use reader authority; never persist or log it.
        grant: BinaryOutputGrant,
    },
}
impl std::fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame").finish_non_exhaustive()
    }
}
/// No default playback, download directory, picker, transcoding, or command fallback.
/// Writes apply backpressure. A sink must bound its device/reader queues and retain spawned
/// work outside each write future, so cancellation of a blocked write never loses its join.
pub trait OutputSink {
    /// Synchronously revoke queued effects before cleanup awaits; retain owners for `finish`.
    fn revoke(&self) {}
    /// Explicit platform/format capability, checked before device capture or admission.
    fn supports(&self, port: &RoutinePort) -> bool;
    /// Recheck the guard immediately before effects. PCM is interleaved little-endian i16.
    /// Read blobs only through the grant's pinned `MediaTransfers`, in at most 64 KiB chunks;
    /// verify reader completion and close it. Wake blocked writes on guard invalidation.
    fn write<'a>(
        &'a self,
        frame: Frame<'a>,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>>;
    /// Stop producers and close readers, discard on failure or drain on success, then join
    /// every local task. Recheck the guard before each drain effect; invalidation discards,
    /// but never skips close/join. Always awaited, even after cancellation. No remote proof implied.
    fn finish<'a>(
        &'a self,
        success: bool,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>>;
}
