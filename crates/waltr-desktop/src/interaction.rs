//! Shared native capture-to-Interact lifecycle, below shortcut and chat consumers.
use crate::{
    capture,
    config::Audio,
    host::{Diagnostic, Error, Observer, RpcFailure, Stage},
    typing::{Guard, LocalFuture},
};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use waltr_client::{
    RoutineInteractionTransport,
    interaction::{InteractionEvent, InteractionLimits, InteractionOutput, wire},
};

/// Exact catalog selection pinned by the host for one access/profile generation.
#[derive(Clone)]
pub struct Invocation {
    /// Current descriptor and discovered server ceilings.
    pub limits: InteractionLimits,
    /// Entrypoint, revision, structured business parameters, correlation, and source context.
    pub selection: wire::InvocationSelection,
}
impl std::fmt::Debug for Invocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invocation").finish_non_exhaustive()
    }
}
impl Invocation {
    /// Pin a current descriptor without inferring eligibility or granting capabilities.
    /// # Errors
    /// Rejects ineligible entrypoints, invalid schema values, or invalid limits.
    pub fn new(
        definition: waltr_client::ClientManualRoutineDefinition,
        server: wire::ServerLimits,
        entrypoint: String,
        parameters: waltr_client::RoutineJson,
        correlation: String,
        source_session: Option<String>,
    ) -> Result<Self, Error> {
        if !definition.permits(&entrypoint, true, source_session.as_deref())
            || correlation.is_empty()
            || correlation.len() > 128
        {
            return Err(Error::Unsupported);
        }
        definition
            .parameters
            .validate(&parameters)
            .map_err(|_| Error::Unsupported)?;
        let selection = wire::InvocationSelection {
            routine: definition.routine_name.clone(),
            revision: definition.routine_revision.clone(),
            entrypoint,
            parameters: Some(wire::StructuredValue {
                json: parameters.as_str().as_bytes().to_vec(),
            }),
            correlation,
            source_session,
        };
        let limits = InteractionLimits::for_definition(definition, server)
            .map_err(|_| Error::Unsupported)?;
        Ok(Self { limits, selection })
    }
}

/// Produce schema-derived initial values for the closed Routine value dialect.
/// # Errors
/// Returns Unsupported when the descriptor or resulting value is invalid.
pub fn default_parameters(
    contract: &waltr_client::RoutineContract,
) -> Result<waltr_client::RoutineJson, Error> {
    fn initial(schema: &waltr_client::RoutineSchema) -> serde_json::Value {
        use waltr_client::RoutineSchema as S;
        match schema {
            S::Null | S::Nullable { .. } => serde_json::Value::Null,
            S::Boolean => false.into(),
            S::Number { min, .. } => serde_json::Value::Number(min.clone()),
            S::String { .. } => "".into(),
            S::Array { .. } => serde_json::Value::Array(Vec::new()),
            S::Object {
                properties,
                required,
            } => serde_json::Value::Object(
                required
                    .iter()
                    .filter_map(|name| properties.get(name).map(|s| (name.clone(), initial(s))))
                    .collect(),
            ),
        }
    }
    let value = waltr_client::RoutineJson::parse(initial(&contract.schema).to_string())
        .map_err(|_| Error::Unsupported)?;
    contract.validate(&value).map_err(|_| Error::Unsupported)?;
    Ok(value)
}

/// Absolute deadline from RPC open through Accepted, independent of PCM backlog capacity.
pub const ACCEPTANCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The selected Routine ports and local capture policy, independent of output consumers.
pub struct Selection<'a> {
    /// Current catalog invocation, pinned for this operation.
    pub invocation: &'a Invocation,
    /// Audio input port.
    pub input: &'a str,
    /// Text output port.
    pub output: &'a str,
    /// Local opt-in capture deadline, in 1..=3600 seconds. None preserves default policy.
    pub max_capture_seconds: Option<u32>,
}
impl std::fmt::Debug for Selection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selection").finish_non_exhaustive()
    }
}
fn validate_capture_selection(
    selection: &Selection<'_>,
    binary: &[wire::InputDeclaration],
) -> Result<(), Error> {
    validate_capture_ports(
        &selection.invocation.limits.definition.ports,
        selection.input,
        selection.output,
        binary,
    )
}
fn validate_capture_ports(
    ports: &[waltr_client::RoutinePort],
    input: &str,
    output: &str,
    binary: &[wire::InputDeclaration],
) -> Result<(), Error> {
    use waltr_client::{RoutineDirection as D, RoutineMedia as M};
    if !ports.iter().any(|p| {
        p.name == input
            && p.direction == D::Input
            && p.media == M::Audio
            && p.formats.iter().any(|f| f == "pcm16le-24000-mono")
    }) || !ports.iter().any(|p| {
        p.name == output
            && p.direction == D::Output
            && ((p.media == M::Text && p.formats.iter().any(|f| f == "utf8"))
                || (p.media == M::Audio && p.formats.iter().all(|f| f == "pcm16le-24000-mono") && !p.formats.is_empty()))
    }) || ports.iter().any(|p| {
        p.required
            && p.direction == D::Input
            && p.name != input
            && !binary.iter().any(|b| b.port == p.name)
    }) {
        return Err(Error::Unsupported);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr_client::{RoutineDirection as D, RoutineMedia as M, RoutinePort, RoutineRevision};

    fn ports() -> Vec<RoutinePort> {
        vec![
            RoutinePort {
                name: "audio".into(),
                direction: D::Input,
                media: M::Audio,
                required: true,
                max_bytes: RoutineRevision(65536),
                formats: vec!["pcm16le-24000-mono".into()],
            },
            RoutinePort {
                name: "transcript".into(),
                direction: D::Output,
                media: M::Text,
                required: true,
                max_bytes: RoutineRevision(65536),
                formats: vec!["utf8".into()],
            },
        ]
    }

    #[test]
    fn production_capture_formats_pass_selection() {
        assert_eq!(
            validate_capture_ports(&ports(), "audio", "transcript", &[]),
            Ok(())
        );
    }

    #[test]
    fn capture_selection_rejects_wrong_contracts() {
        for (index, direction, media, format) in [
            (0, D::Output, M::Audio, "pcm16le-24000-mono"),
            (0, D::Input, M::Text, "pcm16le-24000-mono"),
            (0, D::Input, M::Audio, "pcm16le"),
            (0, D::Input, M::Audio, "pcm16le-48000-mono"),
            (0, D::Input, M::Audio, "pcm16le-24000-stereo"),
            (1, D::Input, M::Text, "utf8"),
            (1, D::Output, M::Audio, "utf8"),
            (1, D::Output, M::Text, "text"),
            (1, D::Output, M::Text, "utf16"),
        ] {
            let mut ports = ports();
            ports[index].direction = direction;
            ports[index].media = media;
            ports[index].formats = vec![format.into()];
            assert_eq!(
                validate_capture_ports(&ports, "audio", "transcript", &[]),
                Err(Error::Unsupported),
                "{index} {direction:?} {media:?} {format}"
            );
        }
    }

    #[test]
    fn capture_selection_rejects_missing_required_input() {
        let mut ports = ports();
        let mut extra = ports[0].clone();
        extra.name = "other-audio".into();
        ports.push(extra);
        assert_eq!(
            validate_capture_ports(&ports, "audio", "transcript", &[]),
            Err(Error::Unsupported)
        );
    }
}
/// Protocol progress shared by status consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Admission acknowledged.
    Accepted,
    /// Capture drained successfully; input is about to close.
    InputClosed,
}
/// Consumer-owned output effects. Calls are backpressured within the RPC receive half.
/// Implementations must recheck the supplied guard immediately before effects.
pub trait Consumer {
    /// Optional native PCM/blob capability. Absence never selects a hidden fallback.
    fn sink(&self) -> Option<&dyn crate::media::OutputSink> {
        None
    }
    /// Publish protocol progress without blocking or performing native effects.
    fn progress(&self, progress: Progress);
    /// Observe validated output state, including the terminal selected text at EOF.
    fn output<'a>(
        &'a self,
        output: &'a InteractionOutput,
        port: &'a str,
        final_text: Option<&'a str>,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>>;
}
/// Successful selected output after both halves and native cleanup complete.
pub struct Output {
    /// Validated successful final text.
    pub text: String,
    /// Accepted invocation identity for command environment.
    pub invocation: String,
}
impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output").finish_non_exhaustive()
    }
}
/// One native capture and interaction lifecycle. The caller retains microphone admission.
/// Cancellation drops RPC work before joining capture. Release is a normal flush, including
/// when it arrives before startup. No nonempty PCM means no RPC or consumer effect.
/// # Errors
/// Returns bounded capture, interaction, consumer, or invalidation errors; cleanup is observed
/// independently even when a primary failure already exists.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::future_not_send
)]
pub async fn run<T: RoutineInteractionTransport>(
    transport: &T,
    factory: Arc<dyn capture::Factory>,
    audio: &Audio,
    selection: Selection<'_>,
    release: &CancellationToken,
    guard: &Guard<'_>,
    observer: &Observer,
    consumer: &dyn Consumer,
) -> Result<Option<Output>, Error> {
    run_with_inputs(
        transport,
        factory,
        audio,
        selection,
        &mut [],
        release,
        guard,
        observer,
        consumer,
    )
    .await
}

/// Capture with additional finite user-selected file/image inputs, declared before admission.
/// Empty capture still performs no RPC or upload. All sources are closed and joined on return.
/// # Errors
/// Returns the same failures as `run`, plus finite source or upload failures.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::future_not_send
)]
pub async fn run_with_inputs<T: RoutineInteractionTransport>(
    transport: &T,
    factory: Arc<dyn capture::Factory>,
    audio: &Audio,
    selection: Selection<'_>,
    inputs: &mut [crate::media::BinaryInput],
    release: &CancellationToken,
    guard: &Guard<'_>,
    observer: &Observer,
    consumer: &dyn Consumer,
) -> Result<Option<Output>, Error> {
    let mut producer = crate::media::Producer::new(inputs);
    // Preflight failures still release every selected local source.
    let preflight = (|| {
        let binary = producer.declarations(selection.invocation)?;
        validate_capture_selection(&selection, &binary)?;
        let open = wire::InteractionOpen {
            selection: Some(selection.invocation.selection.clone()),
            inputs: std::iter::once(wire::InputDeclaration {
                port: selection.input.into(),
                format: Some(wire::input_declaration::Format::Pcm16le(wire::Pcm16Le {
                    sample_rate: audio.sample_rate_hz,
                    channels: u32::from(audio.channels),
                })),
            })
            .chain(binary)
            .collect(),
        };
        selection
            .invocation
            .limits
            .validate_open(&open)
            .map_err(|_| Error::Unsupported)?;
        for port in &selection.invocation.limits.definition.ports {
            if port.direction == waltr_client::RoutineDirection::Output
                && port.media != waltr_client::RoutineMedia::Text
                && !consumer.sink().is_some_and(|sink| sink.supports(port))
            {
                return Err(Error::Unsupported);
            }
        }
        let mut config = capture::native_config(audio).map_err(|_| Error::Unsupported)?;
        let duration = match selection.max_capture_seconds {
            Some(seconds @ 1..=3600) => {
                // Keep the sample ceiling independent of normal wall-clock stop and native flush.
                config.limits.max_duration_seconds = seconds + 60;
                Some(std::time::Duration::from_secs(u64::from(seconds)))
            }
            Some(_) => return Err(Error::Unsupported),
            None => None,
        };
        observer.emit(Diagnostic::Stage(Stage::CaptureRequested));
        guard.check().map_err(|_| Error::Cancelled)?;
        Ok::<_, Error>((open, config, duration))
    })();
    let (open, config, duration) = match preflight {
        Ok(value) => value,
        Err(error) => {
            let cleanup = producer.finish(false).await;
            observer.emit(Diagnostic::SourceCleanup(cleanup));
            return Err(error);
        }
    };
    let mut capture = capture::CaptureWorker::start_observed(factory, config, observer.clone());
    if release.is_cancelled() {
        capture.stop();
    }
    let requested = std::time::Instant::now();
    let started = capture.started_lane();
    let stop = capture.stop_lane();
    let failure = capture.failure_lane();
    let mut session_owner = None;
    let result = {
        let operation = async {
            let first = loop {
                match capture.frames.recv().await {
                    Some(frame) if !frame.is_empty() => break frame,
                    Some(_) => continue,
                    None => {
                        capture.join().await.map_err(Error::Capture)?;
                        return Ok(None);
                    }
                }
            };
            capture.check_failure().map_err(Error::Capture)?;
            observer.emit(Diagnostic::Stage(Stage::FirstFrameObserved));
            guard.check().map_err(|_| Error::Cancelled)?;
            let limits = selection.invocation.limits.clone();
            observer.emit(Diagnostic::Stage(Stage::RpcOpen));
            guard.check().map_err(|_| Error::Cancelled)?;
            let deadline = tokio::time::Instant::now() + ACCEPTANCE_TIMEOUT;
            let session = tokio::time::timeout_at(deadline, transport.interact(open, limits))
                .await
                .map_err(|_| acceptance_timeout(observer))?
                .map_err(|error| rpc(observer, &error, RpcFailure::Open))?;
            session_owner = Some(session);
            let session = session_owner.as_mut().ok_or(Error::Interaction)?;
            let result = duplex(
                session,
                &mut capture,
                audio,
                &selection,
                guard,
                observer,
                consumer,
                first,
                deadline,
                &mut producer,
                &transport.media_transfers(),
            )
            .await;
            result.map(Some)
        };
        tokio::pin!(operation);
        loop {
            tokio::select! { biased;
                () = guard.generation.cancelled() => break Err(Error::Cancelled),
                () = guard.cancel.cancelled() => break Err(Error::Cancelled),
                result = &mut operation => break result,
                () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                    if guard.check().is_err() { break Err(Error::Cancelled); }
                    let failed = *failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(Err(error)) = failed { break Err(Error::Capture(error)); }
                    // A source that has not finished opening still receives a pending stop.
                    // OS initialization itself cannot be interrupted or given a hard join deadline.
                    let expired = duration.is_some_and(|duration| {
                        let start = started.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                            .unwrap_or(requested);
                        start.elapsed() >= duration
                    });
                    if release.is_cancelled() || expired {
                        let _result = stop.compare_exchange(0, 1, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire);
                    }
                }
            }
        }
    };
    if result.is_err() {
        if let Some(sink) = consumer.sink() { sink.revoke(); }
        capture.cancel();
        if let Some(session) = &session_owner {
            session.input.cancel();
        }
    }
    observer.emit(Diagnostic::Primary(
        result.as_ref().map(|_| ()).map_err(|error| *error),
    ));
    observer.emit(Diagnostic::Stage(Stage::Cleanup));
    let cleanup = capture.join().await;
    if cleanup.is_err() || guard.check().is_err() {
        if let Some(sink) = consumer.sink() { sink.revoke(); }
    }
    observer.emit(Diagnostic::CaptureCleanup(cleanup));
    let sources_cleanup = producer
        .finish(result.as_ref().is_ok_and(Option::is_some) && cleanup.is_ok())
        .await;
    observer.emit(Diagnostic::SourceCleanup(sources_cleanup));
    if sources_cleanup.is_err() || guard.check().is_err() {
        if let Some(sink) = consumer.sink() { sink.revoke(); }
    }
    if let Some(session) = &mut session_owner {
        let proof = if result.is_err() {
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
    let sink_cleanup = match consumer.sink() {
        Some(sink) => {
            sink.finish(
                result.is_ok()
                    && cleanup.is_ok()
                    && sources_cleanup.is_ok()
                    && guard.check().is_ok(),
                guard,
            )
            .await
        }
        None => Ok(()),
    };
    if sink_cleanup.is_err() {
        if let Some(sink) = consumer.sink() { sink.revoke(); }
    }
    let output = result?;
    cleanup.map_err(Error::Capture)?;
    sources_cleanup?;
    sink_cleanup?;
    guard.check().map_err(|_| Error::Cancelled)?;
    if output.is_none() {
        observer.emit(Diagnostic::NoInput);
    }
    Ok(output)
}
fn acceptance_timeout(observer: &Observer) -> Error {
    observer.emit(Diagnostic::RpcFailure(RpcFailure::AcceptanceTimeout));
    Error::Interaction
}
fn rpc(observer: &Observer, error: &waltr_client::ClientError, boundary: RpcFailure) -> Error {
    observer.emit(Diagnostic::RpcFailure(RpcFailure::from_client(
        error, boundary,
    )));
    Error::Interaction
}
fn protocol(observer: &Observer, output: &InteractionOutput) -> Error {
    observer.emit(Diagnostic::RpcFailure(RpcFailure::from_output(
        output.output_failure(),
    )));
    Error::Interaction
}
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::future_not_send
)]
async fn duplex(
    session: &mut waltr_client::interaction::InteractionSession,
    capture: &mut capture::CaptureWorker,
    audio: &Audio,
    selection: &Selection<'_>,
    guard: &Guard<'_>,
    observer: &Observer,
    consumer: &dyn Consumer,
    first: Vec<u8>,
    deadline: tokio::time::Instant,
    producer: &mut crate::media::Producer<'_>,
    transfers: &waltr_client::interaction::MediaTransfers,
) -> Result<Output, Error> {
    let (accepted, ready) = tokio::sync::oneshot::channel();
    let input_finished = std::cell::Cell::new(false);
    let receive = async {
        let mut accepted = Some(accepted);
        let mut invocation = None;
        while let Some(event) = session.output.next().await {
            let event = event.map_err(|error| rpc(observer, &error, RpcFailure::Output))?;
            guard.check().map_err(|_| Error::Cancelled)?;
            if let InteractionEvent::Accepted { invocation_id } = &event {
                invocation = Some(invocation_id.clone());
                consumer.progress(Progress::Accepted);
                if let Some(tx) = accepted.take() {
                    observer.emit(Diagnostic::Stage(Stage::Accepted));
                    let _result = tx.send(());
                }
            }
            if let Some(sink) = consumer.sink() {
                let frame = match &event {
                    InteractionEvent::Blob { port, descriptor } => crate::media::Frame::Blob {
                        port,
                        descriptor,
                        grant: session
                            .output
                            .take_blob_grant(port)
                            .ok_or(Error::Interaction)?,
                    },
                    _ => crate::media::Frame::Stream(&event),
                };
                sink.write(frame, guard).await?;
            }
            consumer
                .output(&session.output, selection.output, None, guard)
                .await?;
        }
        let audio_output = selection.invocation.limits.definition.ports.iter().any(|p| {
            p.name == selection.output && p.media == waltr_client::RoutineMedia::Audio
        });
        let text = if audio_output {
            if !session.output.succeeded() {
                return Err(protocol(observer, &session.output));
            }
            String::new()
        } else {
            session.output.final_text(selection.output)
                .map_err(|_| protocol(observer, &session.output))?
        };
        if !input_finished.get() {
            observer.emit(Diagnostic::RpcFailure(RpcFailure::Protocol));
            return Err(Error::Interaction);
        }
        guard.check().map_err(|_| Error::Cancelled)?;
        consumer
            .output(&session.output, selection.output, Some(&text), guard)
            .await?;
        observer.emit(Diagnostic::Stage(Stage::OutputFinished));
        let invocation = invocation.ok_or_else(|| {
            observer.emit(Diagnostic::RpcFailure(RpcFailure::Protocol));
            Error::Interaction
        })?;
        Ok::<_, Error>(Output { text, invocation })
    };
    let send = async {
        tokio::time::timeout_at(deadline, ready)
            .await
            .map_err(|_| acceptance_timeout(observer))?
            .map_err(|_| {
                observer.emit(Diagnostic::RpcFailure(RpcFailure::Protocol));
                Error::Interaction
            })?;
        session
            .input
            .send(wire::input_event::Event::Open(wire::StreamOpen {
                port: selection.input.into(),
                stream_id: "audio".into(),
                format: Some(wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                    sample_rate: audio.sample_rate_hz,
                    channels: u32::from(audio.channels),
                })),
            }))
            .await
            .map_err(|error| rpc(observer, &error, RpcFailure::Input))?;
        let mut pending = Some(first);
        let mut sequence = 0;
        loop {
            let frame = match pending.take() {
                Some(frame) => frame,
                None => match capture.frames.recv().await {
                    Some(frame) => frame,
                    None => break,
                },
            };
            if frame.is_empty() {
                continue;
            }
            session
                .input
                .send(wire::input_event::Event::Chunk(wire::StreamChunk {
                    stream_id: "audio".into(),
                    sequence,
                    payload: Some(wire::stream_chunk::Payload::Audio(frame)),
                }))
                .await
                .map_err(|error| rpc(observer, &error, RpcFailure::Input))?;
            sequence += 1;
        }
        let cleanup = capture.join().await;
        if let Err(error) = cleanup {
            observer.emit(Diagnostic::Primary(Err(Error::Capture(error))));
        }
        observer.emit(Diagnostic::CaptureCleanup(cleanup));
        cleanup.map_err(Error::Capture)?;
        guard.check().map_err(|_| Error::Cancelled)?;
        consumer.progress(Progress::InputClosed);
        session
            .input
            .send(wire::input_event::Event::Close(wire::StreamClose {
                stream_id: "audio".into(),
                next_sequence: sequence,
            }))
            .await
            .map_err(|error| rpc(observer, &error, RpcFailure::Input))?;
        producer.send(&mut session.input, transfers, guard).await?;
        session
            .input
            .finish_input()
            .await
            .map_err(|error| rpc(observer, &error, RpcFailure::Input))?;
        input_finished.set(true);
        observer.emit(Diagnostic::Stage(Stage::InputFinished));
        Ok::<_, Error>(())
    };
    let (output, ()) = tokio::try_join!(receive, send)?;
    Ok(output)
}
