//! Private terminal media owner. Paths and transfer authority never enter snapshots.
#![allow(
    missing_docs,
    reason = "runtime message carriers; not a client or transport API"
)]

mod editor;
mod files;
#[cfg(test)]
mod tests;

pub(crate) use editor::MediaEditor;
use files::{Destination, LocalInput};
use futures_util::{StreamExt, future::LocalBoxFuture};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, timeout, timeout_at},
};
use waltr_client::RoutineInteractionTransport;
use waltr_client::interaction::{
    InteractionEvent, InteractionInputHandle, InteractionLimits, InteractionOutput, MediaTransfers,
    wire,
};
use waltr_client::{
    ClientManualRoutineDefinition, NativeCredentialProvider, NativeGrpcTransport, RoutineLimits,
    RoutineTextResult,
};

const CHUNK: usize = 65536;
const PREVIEW: usize = 65536;
const CLEANUP: Duration = Duration::from_secs(5);
const HEX: &[u8] = b"0123456789abcdef";

/// A trusted, explicit terminal selection, not a server pathname.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSelection {
    pub port: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub pcm24k_mono: bool,
}
impl std::fmt::Debug for LocalSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalSelection([redacted])")
    }
}

/// Finite inputs and explicitly selected output destinations, separate from parameters.
#[derive(Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalPlan {
    #[serde(default)]
    pub inputs: Vec<LocalSelection>,
    #[serde(default)]
    pub saves: BTreeMap<String, String>,
    #[serde(default)]
    pub selected_text: Option<String>,
}
impl std::fmt::Debug for LocalPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalPlan([redacted])")
    }
}

/// Runtime-only request; Debug deliberately omits paths, parameters, and text.
#[derive(Clone, PartialEq, Eq)]
pub enum MediaRequest {
    Copy {
        id: u64,
        text: String,
        truncated: bool,
    },
    Interaction {
        id: u64,
        definition: Box<ClientManualRoutineDefinition>,
        limits: Box<RoutineLimits>,
        entrypoint: String,
        source: Option<String>,
        parameters: String,
        plan: Box<LocalPlan>,
    },
    Text {
        id: u64,
        invocation: String,
        descriptors: Vec<RoutineTextResult>,
        selected: String,
        destination: Option<String>,
    },
}
impl std::fmt::Debug for MediaRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaRequest([private])")
    }
}
impl MediaRequest {
    const fn id(&self) -> u64 {
        match self {
            Self::Interaction { id, .. } | Self::Text { id, .. } | Self::Copy { id, .. } => *id,
        }
    }
}

/// Bounded terminal presentation, with no grants or payload bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaEvent {
    pub id: u64,
    pub status: String,
    pub preview: String,
    pub truncated: bool,
    pub done: bool,
}
impl std::fmt::Debug for MediaEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaEvent")
            .field("id", &self.id)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

fn failure() -> String {
    "Media operation failed; no retry was attempted".into()
}
fn current(credentials: &NativeCredentialProvider, generation: u64) -> Result<(), String> {
    if credentials.credential_lease().generation() == generation {
        Ok(())
    } else {
        Err("Media owner belongs to a previous login".into())
    }
}

pub(crate) struct Broker {
    transport: NativeGrpcTransport,
    credentials: NativeCredentialProvider,
    events: mpsc::Sender<std::io::Result<crate::event::AppEvent>>,
    owner: Option<(u64, watch::Sender<bool>, tokio::task::JoinHandle<()>)>,
}
impl Broker {
    pub(crate) fn new(
        endpoint: String,
        credentials: NativeCredentialProvider,
        events: mpsc::Sender<std::io::Result<crate::event::AppEvent>>,
    ) -> Self {
        Self {
            transport: NativeGrpcTransport::with_credential_provider(endpoint, credentials.clone()),
            credentials,
            events,
            owner: None,
        }
    }
    pub(crate) fn sync_owner(&self, id: Option<u64>) {
        if let Some((owner, cancel, _)) = &self.owner
            && Some(*owner) != id
        {
            cancel.send_replace(true);
        }
    }
    pub(crate) async fn start(&mut self, request: MediaRequest) {
        if self
            .owner
            .as_ref()
            .is_some_and(|(_, _, task)| !task.is_finished())
        {
            self.publish(MediaEvent {
                id: request.id(),
                status: "Previous media cleanup is still running; start again explicitly".into(),
                preview: String::new(),
                truncated: false,
                done: true,
            });
            return;
        }
        if let Some((_, _, task)) = self.owner.take() {
            let _joined = task.await;
        }
        let (cancel, canceled) = watch::channel(false);
        let transport = self.transport.clone();
        let credentials = self.credentials.clone();
        let generation = credentials.credential_lease().generation();
        let events = self.events.clone();
        let id = request.id();
        let copying = matches!(request, MediaRequest::Copy { .. });
        let task = tokio::task::spawn_local(async move {
            let result = execute(request, transport, credentials, generation, canceled).await;
            let event = match result {
                Ok((preview, truncated)) => MediaEvent {
                    id,
                    status: if copying {
                        "Displayed preview copied"
                    } else {
                        "Completed: clean output and local saves verified"
                    }
                    .into(),
                    preview,
                    truncated,
                    done: true,
                },
                Err(status) => MediaEvent {
                    id,
                    status,
                    preview: String::new(),
                    truncated: false,
                    done: true,
                },
            };
            let _sent = events.send(Ok(crate::event::AppEvent::Media(event))).await;
        });
        self.owner = Some((id, cancel, task));
    }
    fn publish(&self, event: MediaEvent) {
        let _sent = self
            .events
            .try_send(Ok(crate::event::AppEvent::Media(event)));
    }
    pub(crate) async fn shutdown(&mut self) {
        if let Some((_, cancel, mut task)) = self.owner.take() {
            cancel.send_replace(true);
            if timeout(CLEANUP * 3, &mut task).await.is_err() {
                task.abort();
                let _joined = task.await;
            }
        }
    }
}

async fn canceled(cancel: &mut watch::Receiver<bool>) {
    while !*cancel.borrow_and_update() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

async fn execute(
    request: MediaRequest,
    transport: NativeGrpcTransport,
    credentials: NativeCredentialProvider,
    generation: u64,
    mut cancel: watch::Receiver<bool>,
) -> Result<(String, bool), String> {
    current(&credentials, generation)?;
    match request {
        MediaRequest::Copy {
            text, truncated, ..
        } => {
            if text.len() > PREVIEW {
                return Err(failure());
            }
            let copied = tokio::select! {
                biased;
                () = canceled(&mut cancel) => return Err("Copy canceled".into()),
                result = timeout(CLEANUP, crate::native_auth::copy_to_clipboard(&text)) => result.unwrap_or(false),
            };
            current(&credentials, generation)?;
            if copied {
                Ok((text, truncated))
            } else {
                Err("Clipboard unavailable".into())
            }
        }
        MediaRequest::Text {
            invocation,
            descriptors,
            selected,
            destination,
            ..
        } => {
            let transfers = transport.media_transfers();
            tokio::select! {
                biased;
                () = canceled(&mut cancel) => Err("Canceled; local text reader released".into()),
                result = timeout(Duration::from_mins(2), read_text(&transfers, &invocation, &descriptors, &selected, destination.as_deref(), &credentials, generation)) => result.map_err(|_| failure())?,
            }
        }
        MediaRequest::Interaction {
            definition,
            limits,
            entrypoint,
            source,
            parameters,
            plan,
            ..
        } => {
            let limits = InteractionLimits::for_definition((*definition).clone(), (*limits).into())
                .map_err(|_| failure())?;
            let (open, inputs) =
                prepare(&definition, &entrypoint, source, parameters, &plan, &limits)?;
            let deadline = Instant::now()
                + Duration::from_millis(limits.server.total_duration_ms.min(900_000));
            current(&credentials, generation)?;
            if *cancel.borrow() {
                return Err("Canceled before admission; no request was started".into());
            }
            let checkout = deadline.min(
                Instant::now()
                    + Duration::from_millis(limits.server.ordinary_deadline_ms.min(30_000)),
            );
            let session = tokio::select! {
                biased;
                () = canceled(&mut cancel) => return Err("Admission checkout canceled; server admission and cleanup are unverified; no retry was attempted".into()),
                result = timeout_at(checkout, transport.interact(open, limits.clone())) => result.map_err(|_| "Admission checkout expired; admission and cleanup are unverified".to_owned())?.map_err(|_| "Admission checkout failed; admission and cleanup are unverified; no retry was attempted".to_owned())?,
            };
            drive(
                session.input,
                session.output,
                transport.media_transfers(),
                inputs,
                *plan,
                credentials,
                generation,
                cancel,
                deadline,
            )
            .await
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "validates exact definition, selection, and declarations together before admission"
)]
fn prepare(
    definition: &ClientManualRoutineDefinition,
    entrypoint: &str,
    source: Option<String>,
    parameters: String,
    plan: &LocalPlan,
    limits: &InteractionLimits,
) -> Result<(wire::InteractionOpen, Vec<LocalInput>), String> {
    if plan.inputs.len() > limits.server.streams_per_direction as usize {
        return Err("Too many local input declarations".into());
    }
    let mut ports = std::collections::BTreeSet::new();
    if plan.inputs.iter().any(|s| !ports.insert(s.port.as_str())) {
        return Err("Duplicate local input port".into());
    }
    let mut inputs = Vec::new();
    let mut declarations = Vec::new();
    let mut bytes = 0u64;
    for selection in &plan.inputs {
        let input = LocalInput::open(selection, definition, limits)?;
        bytes = bytes
            .checked_add(input.bytes())
            .filter(|b| *b <= limits.server.input_bytes)
            .ok_or("Local inputs exceed the total input budget")?;
        declarations.push(input.declaration.clone());
        inputs.push(input);
    }
    for port in plan.saves.keys().chain(plan.selected_text.iter()) {
        let output = definition
            .ports
            .iter()
            .find(|p| p.name == *port && p.direction == waltr_client::RoutineDirection::Output)
            .ok_or_else(failure)?;
        if plan.selected_text.as_ref() == Some(port)
            && output.media != waltr_client::RoutineMedia::Text
        {
            return Err(failure());
        }
    }
    let open = wire::InteractionOpen {
        selection: Some(wire::InvocationSelection {
            routine: definition.routine_name.clone(),
            revision: definition.routine_revision.clone(),
            entrypoint: entrypoint.into(),
            source_session: source,
            parameters: Some(wire::StructuredValue {
                json: parameters.into_bytes(),
            }),
            correlation: format!("tui-{}", editor::next_id()),
        }),
        inputs: declarations,
    };
    limits.validate_open(&open).map_err(|_| {
        "Invalid parameters, input declarations, formats, or required ports".to_owned()
    })?;
    Ok((open, inputs))
}

#[allow(
    clippy::too_many_arguments,
    reason = "one owned interaction with explicit independent fences"
)]
async fn drive(
    mut input: InteractionInputHandle,
    mut output: InteractionOutput,
    transfers: MediaTransfers,
    inputs: Vec<LocalInput>,
    plan: LocalPlan,
    credentials: NativeCredentialProvider,
    generation: u64,
    mut cancel: watch::Receiver<bool>,
    deadline: Instant,
) -> Result<(String, bool), String> {
    let control = output.control();
    let (stop, stopped) = watch::channel(false);
    let mut rejected = false;
    let result = async {
        match output.next().await {
            Some(Ok(InteractionEvent::Accepted { .. })) => {}
            Some(Ok(InteractionEvent::Rejected { code })) => {
                rejected = true;
                return Err(format!(
                    "Admission rejected (code {code}); no invocation was admitted"
                ));
            }
            _ => return Err(failure()),
        }
        current(&credentials, generation)?;
        let limits = input.accepted_limits().map_err(|_| failure())?;
        let producer = async {
            let result = send_inputs(
                &mut input,
                &transfers,
                inputs,
                &credentials,
                generation,
                limits.chunk_bytes,
            )
            .await;
            if result.is_err() {
                control.cancel();
                stop.send_replace(true);
            }
            result
        };
        let consumer = async {
            let result = consume(
                &mut output,
                &transfers,
                &plan,
                &limits,
                &credentials,
                generation,
                deadline,
                stopped,
            )
            .await;
            if result.is_err() {
                control.cancel();
                stop.send_replace(true);
            }
            result
        };
        let (sent, received) = futures_util::join!(producer, consumer);
        prefer_cleanup(sent, received)
    };
    let result = {
        tokio::pin!(result);
        let finished = tokio::select! {
            biased;
            () = canceled(&mut cancel) => None,
            () = tokio::time::sleep_until(deadline) => None,
            result = &mut result => Some(result),
        };
        if let Some(finished) = finished {
            finished
        } else {
            control.cancel();
            stop.send_replace(true);
            match timeout(CLEANUP * 2, &mut result).await {
                Err(_) => Err("Local media join timed out; remote cleanup is unverified".into()),
                Ok(Err(error)) if error.contains("unverified") => Err(error),
                _ => Err("Canceled or deadline expired; business success was not reported".into()),
            }
        }
    };
    if result.is_err() && !rejected {
        control.cancel();
        let proof = timeout(CLEANUP, output.cancel_and_join()).await;
        if !matches!(proof, Ok(Ok(true))) {
            return Err("Media stopped locally; remote cleanup is unverified".into());
        }
    }
    let (preview, truncated, saves) = result?;
    for mut save in saves {
        current(&credentials, generation)?;
        save.publish()?;
    }
    Ok((preview, truncated))
}

fn prefer_cleanup<T>(sent: Result<(), String>, consumed: Result<T, String>) -> Result<T, String> {
    if matches!(&consumed, Err(error) if error.contains("unverified")) {
        consumed
    } else {
        sent.and(consumed)
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "separate bounded input owner and credential fence"
)]
async fn send_inputs(
    input: &mut InteractionInputHandle,
    transfers: &MediaTransfers,
    inputs: Vec<LocalInput>,
    credentials: &NativeCredentialProvider,
    generation: u64,
    chunk_bytes: usize,
) -> Result<(), String> {
    use wire::{input_event::Event, stream_chunk::Payload, stream_open::Format};
    if chunk_bytes < 256 {
        return Err("Negotiated stream chunks are too small for terminal input".into());
    }
    for mut source in inputs {
        current(credentials, generation)?;
        if matches!(
            source.declaration.format,
            Some(wire::input_declaration::Format::Binary(_))
        ) {
            let mut upload = input
                .begin_upload(transfers, &source.declaration.port)
                .map_err(|_| failure())?;
            let result = async {
                upload.begin().await.map_err(|_| failure())?;
                let mut buffer = vec![0; (upload.chunk_bytes() as usize).min(CHUNK)];
                loop {
                    current(credentials, generation)?;
                    let n = source.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    upload.append(&buffer[..n]).await.map_err(|_| failure())?;
                }
                let (_, event) = upload.commit().await.map_err(|_| failure())?;
                input
                    .send(event.event.ok_or_else(failure)?)
                    .await
                    .map_err(|_| failure())
            }
            .await;
            if result.is_err() && !matches!(timeout(CLEANUP, upload.cancel()).await, Ok(Ok(()))) {
                return Err("Upload cancellation is unverified".into());
            }
            result?;
        } else {
            let stream = format!("input-{}", editor::next_id());
            let format = match source.declaration.format.clone() {
                Some(wire::input_declaration::Format::Text(t)) => Format::Text(t),
                Some(wire::input_declaration::Format::Pcm16le(p)) => Format::Pcm16le(p),
                _ => return Err(failure()),
            };
            input
                .send(Event::Open(wire::StreamOpen {
                    port: source.declaration.port.clone(),
                    stream_id: stream.clone(),
                    format: Some(format),
                }))
                .await
                .map_err(|_| failure())?;
            let mut sequence = 0;
            if let Some(text) = source.text.take() {
                let max = limits_text_chunk(&text, chunk_bytes.saturating_sub(128));
                let count = max.len();
                for (index, chunk) in max.into_iter().enumerate() {
                    current(credentials, generation)?;
                    input
                        .send(Event::Chunk(wire::StreamChunk {
                            stream_id: stream.clone(),
                            sequence,
                            payload: Some(Payload::Text(wire::TextRevision {
                                segment_id: "input-text".into(),
                                mutation: 1,
                                text: chunk,
                                committed: index + 1 == count,
                            })),
                        }))
                        .await
                        .map_err(|_| failure())?;
                    sequence += 1;
                }
            } else {
                let mut bytes = vec![0; chunk_bytes / 2 * 2];
                loop {
                    current(credentials, generation)?;
                    let n = source.read(&mut bytes)?;
                    if n == 0 {
                        break;
                    }
                    if n % 2 != 0 {
                        return Err("Raw PCM must contain complete 16-bit mono frames".into());
                    }
                    input
                        .send(Event::Chunk(wire::StreamChunk {
                            stream_id: stream.clone(),
                            sequence,
                            payload: Some(Payload::Audio(bytes[..n].to_vec())),
                        }))
                        .await
                        .map_err(|_| failure())?;
                    sequence += 1;
                }
            }
            input
                .send(Event::Close(wire::StreamClose {
                    stream_id: stream,
                    next_sequence: sequence,
                }))
                .await
                .map_err(|_| failure())?;
        }
    }
    input.finish_input().await.map_err(|_| failure())
}

fn limits_text_chunk(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut remaining = text;
    let mut result = Vec::new();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(max);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        result.push(remaining[..end].to_owned());
        remaining = &remaining[end..];
    }
    result
}

#[allow(
    clippy::too_many_arguments,
    reason = "output owns its separate absolute transfer deadline and credential fence"
)]
async fn consume(
    output: &mut InteractionOutput,
    transfers: &MediaTransfers,
    plan: &LocalPlan,
    limits: &InteractionLimits,
    credentials: &NativeCredentialProvider,
    generation: u64,
    deadline: Instant,
    mut stopped: watch::Receiver<bool>,
) -> Result<(String, bool, Vec<Destination>), String> {
    use wire::{stream_chunk::Payload, stream_open::Format};
    let mut audio: BTreeMap<String, Destination> = BTreeMap::new();
    let mut downloads: futures_util::stream::FuturesUnordered<
        LocalBoxFuture<'_, Result<Destination, String>>,
    > = futures_util::stream::FuturesUnordered::new();
    let mut completed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut eof = false;
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(
        (limits.server.blob_readers as usize).min(4),
    ));
    let (downloads_stop, downloads_stopped) = watch::channel(false);
    let outcome = async {
    while !eof || !downloads.is_empty() {
        current(credentials, generation)?;
        tokio::select! {
            biased;
            () = canceled(&mut stopped) => return Err("Media output canceled".into()),
            result = downloads.next(), if !downloads.is_empty() => { completed.push(result.ok_or_else(failure)??); }
            event = output.next(), if !eof => {
                match event {
                    None => eof = true,
                    Some(Err(_)) => return Err(failure()),
                    Some(Ok(InteractionEvent::Open(stream))) => {
                        if matches!(stream.format, Some(Format::Pcm16le(_))) && let Some(path) = plan.saves.get(&stream.port) {
                            if !seen.insert(stream.port.clone()) { return Err(failure()); }
                            audio.insert(stream.stream_id, Destination::new(path)?);
                        }
                    }
                    Some(Ok(InteractionEvent::Chunk(chunk))) => {
                        if let Some(Payload::Audio(bytes)) = chunk.payload && let Some(save) = audio.get_mut(&chunk.stream_id) { save.write(&bytes)?; }
                    }
                    Some(Ok(InteractionEvent::Blob { port, descriptor })) => {
                        let issued = Instant::now();
                        if let Some(path) = plan.saves.get(&port) {
                            if !seen.insert(port.clone()) { return Err(failure()); }
                            let grant = output.take_blob_grant(&port).ok_or_else(failure)?;
                            let path = path.clone();
                            let expiry = issued + Duration::from_millis(descriptor.remaining_validity_ms.ok_or_else(failure)?.0.min(900_000));
                            let absolute = expiry.min(deadline);
                            let checkout = absolute.min(Instant::now() + Duration::from_millis(limits.server.ordinary_deadline_ms.min(30_000)));
                            let mut stopped = downloads_stopped.clone();
                            let slots = slots.clone();
                            downloads.push(Box::pin(async move {
                                let _slot = tokio::select! {
                                    biased;
                                    () = canceled(&mut stopped) => return Err("Binary save canceled before reader checkout".into()),
                                    result = timeout_at(checkout, slots.acquire_owned()) => result.map_err(|_| "Binary reader checkout deadline expired before acquisition".to_owned())?.map_err(|_| failure())?,
                                };
                                let mut reader = tokio::select! {
                                    biased;
                                    () = canceled(&mut stopped) => return Err("Reader checkout canceled; lease cleanup is unverified".into()),
                                    result = timeout_at(checkout, transfers.read_output(grant)) => result.map_err(|_| "Reader checkout expired; cleanup is unverified".to_owned())?.map_err(|_| "Reader checkout failed; cleanup is unverified".to_owned())?,
                                };
                                let result = async {
                                    let actual = &reader.descriptor;
                                    if actual.blob_id != descriptor.blob_id || actual.bytes != descriptor.bytes || actual.sha256 != descriptor.sha256 || actual.content_type != descriptor.content_type || actual.image != descriptor.image || actual.display_name != descriptor.display_name { return Err(failure()); }
                                    let save = copy_verified(&mut reader, &path, descriptor.bytes.0, &descriptor.sha256, || current(credentials, generation)).await?;
                                    if !reader.complete() { return Err(failure()); }
                                    Ok(save)
                                };
                                let result = tokio::select! {
                                    biased;
                                    () = canceled(&mut stopped) => Err("Binary save canceled".into()),
                                    result = timeout_at(absolute, result) => result.map_err(|_| "Binary output expired".to_owned()).and_then(|r| r),
                                };
                                let closed = timeout(CLEANUP, reader.close()).await;
                                if !matches!(closed, Ok(Ok(()))) { return Err("Binary reader close is unverified".into()); }
                                result
                            }));
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    Ok::<(), String>(())
    }.await;
    if let Err(error) = outcome {
        downloads_stop.send_replace(true);
        let mut cleanup_error = None;
        while let Some(result) = downloads.next().await {
            if let Err(cleanup) = result
                && cleanup.contains("unverified")
            {
                cleanup_error = Some(cleanup);
            }
        }
        return Err(cleanup_error.unwrap_or(error));
    }
    if !output.succeeded() {
        return Err("No successful terminal and clean EOF".into());
    }
    current(credentials, generation)?;
    let mut preview = String::new();
    let mut truncated = false;
    for port in limits.definition.ports.iter().filter(|p| {
        p.direction == waltr_client::RoutineDirection::Output
            && p.media == waltr_client::RoutineMedia::Text
    }) {
        if let Ok(text) = output.final_text(&port.name) {
            if let Some(path) = plan.saves.get(&port.name) {
                if !seen.insert(port.name.clone()) {
                    return Err(failure());
                }
                let mut save = Destination::new(path)?;
                for bytes in text.as_bytes().chunks(CHUNK) {
                    save.write(bytes)?;
                }
                completed.push(save);
            }
            if plan.selected_text.as_ref() == Some(&port.name) {
                (preview, truncated) = bounded_preview(&text);
            }
        }
    }
    if plan.saves.keys().any(|p| !seen.contains(p)) {
        return Err("Selected output was not produced; no save published".into());
    }
    completed.extend(audio.into_values());
    Ok((preview, truncated, completed))
}

#[allow(
    clippy::too_many_arguments,
    reason = "selected durable descriptor and separate generation-fenced destination"
)]
async fn read_text(
    transfers: &MediaTransfers,
    invocation: &str,
    descriptors: &[RoutineTextResult],
    selected: &str,
    destination: Option<&str>,
    credentials: &NativeCredentialProvider,
    generation: u64,
) -> Result<(String, bool), String> {
    let descriptor = descriptors
        .iter()
        .find(|d| d.id == selected)
        .ok_or_else(failure)?;
    let mut reader = transfers
        .text_reader(invocation.to_owned(), descriptors, selected)
        .map_err(|_| failure())?;
    let mut save = destination.map(Destination::new).transpose()?;
    let mut preview = String::new();
    let mut truncated = false;
    let mut bytes = 0u64;
    let mut hash = Sha256::new();
    while let Some(text) = reader.next_chunk().await.map_err(|_| failure())? {
        current(credentials, generation)?;
        bytes += text.len() as u64;
        hash.update(text.as_bytes());
        if let Some(save) = &mut save {
            for bytes in text.as_bytes().chunks(CHUNK) {
                save.write(bytes)?;
            }
        }
        let (part, overflow) = bounded_preview_to(&text, PREVIEW.saturating_sub(preview.len()));
        preview.push_str(&part);
        truncated |= overflow;
    }
    let digest: String = hash
        .finalize()
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect();
    if bytes != descriptor.utf8_bytes.0 || digest != descriptor.sha256 {
        return Err("Text result digest or byte length mismatch".into());
    }
    current(credentials, generation)?;
    if let Some(save) = &mut save {
        save.publish()?;
    }
    Ok((preview, truncated))
}

fn bounded_preview(text: &str) -> (String, bool) {
    bounded_preview_to(text, PREVIEW)
}
fn bounded_preview_to(text: &str, max: usize) -> (String, bool) {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), end != text.len())
}

async fn copy_verified<S>(
    source: &mut S,
    path: &str,
    expected_bytes: u64,
    expected_sha: &[u8],
    guard: impl Fn() -> Result<(), String>,
) -> Result<Destination, String>
where
    S: futures_util::Stream<Item = Result<Vec<u8>, waltr_client::ClientError>> + Unpin,
{
    guard()?;
    let mut save = Destination::new(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    while let Some(chunk) = source.next().await {
        guard()?;
        let chunk = chunk.map_err(|_| failure())?;
        if chunk.len() > CHUNK {
            return Err("Binary chunk exceeds 64 KiB".into());
        }
        bytes = bytes
            .checked_add(chunk.len() as u64)
            .filter(|n| *n <= expected_bytes)
            .ok_or_else(failure)?;
        hash.update(&chunk);
        save.write(&chunk)?;
    }
    guard()?;
    if bytes != expected_bytes || hash.finalize().as_slice() != expected_sha {
        return Err("Binary digest or byte length mismatch".into());
    }
    Ok(save)
}
