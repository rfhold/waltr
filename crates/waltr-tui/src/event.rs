//! App-level events consumed by the TUI state machine.

use std::time::Instant;

use waltr_client::ClientSnapshot;

use crate::input::InputEvent;

/// Event emitted by runtime producers and handled by the app.
#[derive(Clone, Debug, PartialEq)]
pub enum AppEvent {
    /// Normalized terminal input.
    Input(InputEvent),
    /// Time tick used for expiring transient state.
    Tick(Instant),
    /// Streaming text or status update from an async producer.
    Stream(StreamEvent),
    /// Shared client state changed.
    Client(Box<ClientSnapshot>),
}

impl AppEvent {
    /// Return the event timestamp to use for state updates.
    #[must_use]
    pub const fn timestamp(&self, fallback: Instant) -> Instant {
        match self {
            Self::Tick(now) => *now,
            Self::Input(_) | Self::Stream(_) | Self::Client(_) => fallback,
        }
    }
}

/// Async stream update delivered to the app event loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent {
    /// A chunk of streamed text is available.
    Chunk {
        /// Runtime-local stream identifier.
        stream_id: u64,
        /// Text chunk to append to the stream buffer.
        text: String,
    },
    /// A stream completed successfully.
    Finished {
        /// Runtime-local stream identifier.
        stream_id: u64,
    },
    /// A stream failed.
    Error {
        /// Runtime-local stream identifier.
        stream_id: u64,
        /// Human-readable error message.
        message: String,
    },
}
