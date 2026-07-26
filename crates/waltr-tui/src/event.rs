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
    /// Secret-free progress from the native Waltr login runtime.
    NativeLogin(NativeLoginEvent),
}

impl AppEvent {
    /// Return the event timestamp to use for state updates.
    #[must_use]
    pub const fn timestamp(&self, fallback: Instant) -> Instant {
        match self {
            Self::Tick(now) => *now,
            Self::Input(_) | Self::Stream(_) | Self::Client(_) | Self::NativeLogin(_) => fallback,
        }
    }
}

/// Secret-free native Waltr login progress.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeLoginEvent {
    /// Metadata discovery is in progress.
    Discovering,
    /// The authorization URL is ready and the callback listener is active.
    AwaitingBrowser {
        /// URL that may be copied when browser launching is unavailable.
        authorization_url: String,
        /// Callback deadline.
        expires_at: Instant,
        /// Whether a platform browser command was started successfully.
        browser_opened: bool,
    },
    /// The platform browser command started but later reported failure.
    BrowserLaunchFailed,
    /// The authorization code is being exchanged.
    Exchanging,
    /// The access token is being refreshed.
    Refreshing,
    /// Refresh completed and credentials were atomically replaced.
    RefreshSucceeded,
    /// Credentials were installed before client reconnection.
    Succeeded,
    /// The flow failed with safe user-facing text.
    Failed {
        /// Safe failure text; never contains token response bodies.
        error: String,
    },
    /// The flow was canceled.
    Canceled,
    /// In-memory credentials were removed.
    LoggedOut,
    /// Clipboard command completed.
    UrlCopyFinished {
        /// Whether a supported platform clipboard command succeeded.
        copied: bool,
    },
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
