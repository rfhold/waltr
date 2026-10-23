//! Shared native capture-to-Interact lifecycle, below shortcut and chat consumers.
use crate::{
    capture,
    config::Audio,
    host::{Diagnostic, Error, Observer, RpcFailure, Stage},
    typing::{Guard, LocalFuture},
};
use futures_util::StreamExt;
use waltr_client::{
    RoutineInteractionTransport,
    interaction::{InteractionLimits, InteractionOutput, wire},
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The selected Routine ports, independent of shortcut configuration.
pub struct Selection<'a> {
    /// Routine name.
    pub routine: &'a str,
    /// Trigger name.
    pub trigger: &'a str,
    /// Audio input port.
    pub input: &'a str,
    /// Text output port.
    pub output: &'a str,
}
impl std::fmt::Debug for Selection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selection").finish_non_exhaustive()
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
    let config = capture::native_config(audio).map_err(|_| Error::Unsupported)?;
    guard.check().map_err(|_| Error::Cancelled)?;
    observer.emit(Diagnostic::Stage(Stage::CaptureRequested));
    guard.check().map_err(|_| Error::Cancelled)?;
    let mut capture = capture::CaptureWorker::start_observed(factory, config, observer.clone());
    if release.is_cancelled() {
        capture.stop();
    }
    let stop = capture.stop_lane();
    let failure = capture.failure_lane();
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
            let limits = InteractionLimits {
                input_bytes: audio.sample_rate_hz as usize / 2,
                input_items: 4,
                chunk_bytes: 256 * 1024,
                text_bytes: 256 * 1024,
                ..Default::default()
            };
            observer.emit(Diagnostic::Stage(Stage::RpcOpen));
            guard.check().map_err(|_| Error::Cancelled)?;
            let mut session = transport
                .interact(
                    wire::OpenInteraction {
                        routine_name: selection.routine.into(),
                        trigger_name: selection.trigger.into(),
                        ..Default::default()
                    },
                    limits,
                )
                .await
                .map_err(|error| rpc(observer, &error, RpcFailure::Open))?;
            duplex(
                &mut session,
                &mut capture,
                audio,
                &selection,
                guard,
                observer,
                consumer,
                first,
            )
            .await
            .map(Some)
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
                    if release.is_cancelled() {
                        let _result = stop.compare_exchange(0, 1, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire);
                    }
                }
            }
        }
    };
    if result.is_err() {
        capture.cancel();
    }
    observer.emit(Diagnostic::Primary(
        result.as_ref().map(|_| ()).map_err(|error| *error),
    ));
    observer.emit(Diagnostic::Stage(Stage::Cleanup));
    let cleanup = capture.join().await;
    observer.emit(Diagnostic::CaptureCleanup(cleanup));
    let output = result?;
    cleanup.map_err(Error::Capture)?;
    guard.check().map_err(|_| Error::Cancelled)?;
    if output.is_none() {
        observer.emit(Diagnostic::NoInput);
    }
    Ok(output)
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
) -> Result<Output, Error> {
    let (accepted, ready) = tokio::sync::oneshot::channel();
    let input_finished = std::cell::Cell::new(false);
    let receive = async {
        let mut accepted = Some(accepted);
        let mut invocation = None;
        while let Some(event) = session.output.next().await {
            let event = event.map_err(|error| rpc(observer, &error, RpcFailure::Output))?;
            guard.check().map_err(|_| Error::Cancelled)?;
            if let Some(wire::interact_response::Event::Accepted(value)) = event.event {
                invocation = Some(value.invocation_id);
                consumer.progress(Progress::Accepted);
                if let Some(tx) = accepted.take() {
                    observer.emit(Diagnostic::Stage(Stage::Accepted));
                    let _result = tx.send(());
                }
            }
            consumer
                .output(&session.output, selection.output, None, guard)
                .await?;
        }
        let text = session
            .output
            .final_text(selection.output)
            .map_err(|_| protocol(observer, &session.output))?;
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
        ready.await.map_err(|_| {
            observer.emit(Diagnostic::RpcFailure(RpcFailure::Protocol));
            Error::Interaction
        })?;
        session
            .input
            .send(wire::interact_request::Event::InputOpened(
                wire::InteractionInputOpened {
                    port_name: selection.input.into(),
                    stream_id: "audio".into(),
                    format: Some(wire::InteractionStreamFormat {
                        content: Some(wire::interaction_stream_format::Content::Audio(
                            wire::InteractionAudioFormat {
                                encoding: 1,
                                sample_rate_hz: audio.sample_rate_hz,
                                channels: u32::from(audio.channels),
                            },
                        )),
                    }),
                },
            ))
            .await
            .map_err(|error| rpc(observer, &error, RpcFailure::Input))?;
        let mut pending = Some(first);
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
                .send(wire::interact_request::Event::InputChunk(
                    wire::InteractionInputChunk {
                        stream_id: "audio".into(),
                        content: Some(wire::interaction_input_chunk::Content::Audio(frame)),
                    },
                ))
                .await
                .map_err(|error| rpc(observer, &error, RpcFailure::Input))?;
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
            .send(wire::interact_request::Event::InputClosed(
                wire::InteractionInputClosed {
                    stream_id: "audio".into(),
                    status: 1,
                    failure: None,
                },
            ))
            .await
            .map_err(|error| rpc(observer, &error, RpcFailure::Input))?;
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
