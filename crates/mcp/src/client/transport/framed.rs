use std::{
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
};

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{TransportInbound, request_cancelled};
use crate::{
    Error, Result,
    protocol::{JsonRpcInbound, parse_json_rpc_message},
};

const INBOUND_CAPACITY: usize = 16;

/// An already-open, reliable, ordered, bidirectional channel of MCP JSON-RPC frames.
///
/// Each frame contains one UTF-8 JSON-RPC message with no literal CR or LF.
/// For compatibility with CRLF line readers, inbound frames may end in one CR.
/// The adapter does not authenticate, reconnect, or retry the channel. It calls
/// at most one send and one receive concurrently. The caller must not use the
/// channel concurrently outside the adapter.
pub trait McpFramedChannel: Send + Sync + 'static {
    /// Send one frame. Dropping the future must leave the channel usable with
    /// no partial frame. An error or cancellation does not prove that the
    /// peer did not receive the frame.
    fn send_frame(&self, frame: Vec<u8>) -> BoxFuture<'_, Result<()>>;

    /// Receive one frame, or `Ok(None)` at EOF. Dropping this future must be
    /// safe. Apply framing/allocation bounds in the channel itself; the MCP
    /// client checks the frame size after receipt, before decoding.
    fn recv_frame(&self) -> BoxFuture<'_, Result<Option<Vec<u8>>>>;

    /// Clean up the channel after send and receive futures have stopped.
    /// Return success only when the cleanup promised by this implementation is
    /// complete; the MCP adapter cannot infer remote cleanup from EOF or drop.
    fn shutdown(&self) -> BoxFuture<'_, Result<()>>;
}

pub(in crate::client) struct FramedTransport {
    channel: Arc<dyn McpFramedChannel>,
    state: Arc<State>,
    writer: Arc<Mutex<()>>,
    cleanup: Mutex<Cleanup>,
    max_message_bytes: NonZeroUsize,
}

enum Cleanup {
    NotStarted(JoinHandle<()>),
    Running(JoinHandle<Result<()>>),
    Finished(std::result::Result<(), String>),
}

struct State {
    stopped: CancellationToken,
    overflowed: CancellationToken,
    disconnect: OnceLock<String>,
}

impl State {
    fn disconnect(&self, reason: String) {
        self.disconnect.get_or_init(|| reason);
        self.stopped.cancel();
    }

    fn overflow(&self) {
        self.disconnect
            .get_or_init(|| "MCP framed inbound backlog is full".to_owned());
        self.overflowed.cancel();
        self.stopped.cancel();
    }

    fn closed_error(&self) -> Error {
        Error::protocol(
            self.disconnect
                .get()
                .map_or("MCP framed transport is shut down", String::as_str),
        )
    }
}

pub(in crate::client) struct FramedInbound {
    state: Arc<State>,
    messages: mpsc::Receiver<JsonRpcInbound>,
    terminated: bool,
}

impl FramedInbound {
    pub(in crate::client) async fn recv(&mut self) -> Option<TransportInbound> {
        if self.terminated {
            return None;
        }
        // Preserve queued replies before EOF, but abandon the backlog on overflow.
        if !self.state.overflowed.is_cancelled() {
            tokio::select! {
                biased;
                message = self.messages.recv() => {
                    if let Some(message) = message {
                        return Some(TransportInbound::Message(message));
                    }
                    if !self.state.stopped.is_cancelled() {
                        self.state.disconnect("MCP framed reader stopped unexpectedly".into());
                    }
                }
                () = self.state.stopped.cancelled() => {
                    if !self.state.overflowed.is_cancelled()
                        && let Ok(message) = self.messages.try_recv()
                    {
                        return Some(TransportInbound::Message(message));
                    }
                }
            }
        }
        self.terminated = true;
        self.messages.close();
        self.state
            .disconnect
            .get()
            .map(|reason| TransportInbound::Disconnected(reason.clone()))
    }
}

impl FramedTransport {
    pub(in crate::client) fn new(
        channel: Arc<dyn McpFramedChannel>,
        max_message_bytes: NonZeroUsize,
    ) -> (Self, FramedInbound) {
        let (inbound, messages) = mpsc::channel(INBOUND_CAPACITY);
        let state = Arc::new(State {
            stopped: CancellationToken::new(),
            overflowed: CancellationToken::new(),
            disconnect: OnceLock::new(),
        });
        let reader = tokio::spawn(read_frames(
            Arc::clone(&channel),
            Arc::clone(&state),
            inbound,
            max_message_bytes,
        ));
        (
            Self {
                channel,
                state: Arc::clone(&state),
                writer: Arc::new(Mutex::new(())),
                cleanup: Mutex::new(Cleanup::NotStarted(reader)),
                max_message_bytes,
            },
            FramedInbound {
                state,
                messages,
                terminated: false,
            },
        )
    }

    pub(in crate::client) async fn send(
        &self,
        message: Value,
        cancellation: Option<&CancellationToken>,
    ) -> Result<()> {
        let frame = serde_json::to_vec(&message)?;
        if frame.len() > self.max_message_bytes.get() {
            return Err(Error::protocol(
                "outbound MCP frame exceeds the configured limit",
            ));
        }

        tokio::select! {
            biased;
            () = request_cancelled(cancellation) => Err(Error::protocol("MCP request canceled")),
            () = self.state.stopped.cancelled() => Err(self.state.closed_error()),
            result = async {
                let _writer = self.writer.lock().await;
                let result = self.channel.send_frame(frame).await;
                if let Err(error) = &result {
                    self.state.disconnect(format!("failed to send MCP frame: {error}"));
                }
                result
            } => result,
        }
    }

    pub(in crate::client) async fn backlog_overflowed(&self) -> Error {
        self.state.overflowed.cancelled().await;
        self.state.closed_error()
    }

    pub(in crate::client) async fn shutdown(&self) -> Result<()> {
        self.state.stopped.cancel();
        let mut cleanup = self.cleanup.lock().await;
        if matches!(*cleanup, Cleanup::NotStarted(_)) {
            let Cleanup::NotStarted(reader) =
                std::mem::replace(&mut *cleanup, Cleanup::Finished(Ok(())))
            else {
                unreachable!()
            };
            let channel = Arc::clone(&self.channel);
            let writer = Arc::clone(&self.writer);
            *cleanup = Cleanup::Running(tokio::spawn(async move {
                let _writer = writer.lock().await;
                let read_result = reader.await;
                channel.shutdown().await?;
                read_result.map_err(|error| {
                    Error::protocol(format!("MCP framed reader task failed: {error}"))
                })
            }));
        }
        let result = match &mut *cleanup {
            Cleanup::NotStarted(_) => unreachable!(),
            Cleanup::Finished(result) => return result.clone().map_err(Error::protocol),
            Cleanup::Running(task) => task.await,
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => Err(Error::protocol(format!(
                "MCP framed shutdown task failed: {error}"
            ))),
        };
        *cleanup = Cleanup::Finished(result.as_ref().copied().map_err(ToString::to_string));
        result
    }
}

impl Drop for FramedTransport {
    fn drop(&mut self) {
        self.state.stopped.cancel();
    }
}

async fn read_frames(
    channel: Arc<dyn McpFramedChannel>,
    state: Arc<State>,
    inbound: mpsc::Sender<JsonRpcInbound>,
    max_message_bytes: NonZeroUsize,
) {
    let reason = loop {
        let frame = tokio::select! {
            biased;
            () = state.stopped.cancelled() => return,
            () = inbound.closed() => return,
            result = channel.recv_frame() => result,
        };
        let frame = match frame {
            Ok(Some(frame)) => frame,
            Ok(None) => break "MCP framed channel reached EOF".to_owned(),
            Err(error) => break format!("failed to receive MCP frame: {error}"),
        };
        if frame.len() > max_message_bytes.get() {
            break "inbound MCP frame exceeds the configured limit".to_owned();
        }
        let frame = frame.strip_suffix(b"\r").unwrap_or(&frame);
        if frame.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
            break "MCP frame contains a line ending".to_owned();
        }
        let message = match serde_json::from_slice(frame)
            .map_err(Error::from)
            .and_then(parse_json_rpc_message)
        {
            Ok(message) => message,
            Err(error) => break format!("invalid MCP framed JSON-RPC message: {error}"),
        };
        match inbound.try_send(message) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                state.overflow();
                return;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    };
    state.disconnect(reason);
}
