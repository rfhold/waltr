//! App-level events consumed by the TUI state machine.

use std::{fmt, time::Instant};

use qrcodegen::{QrCode, QrCodeEcc};
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
    /// Bounded presentation from the private media owner.
    Media(crate::media::MediaEvent),
}

impl AppEvent {
    /// Return the event timestamp to use for state updates.
    #[must_use]
    pub const fn timestamp(&self, fallback: Instant) -> Instant {
        match self {
            Self::Tick(now) => *now,
            Self::Input(_)
            | Self::Stream(_)
            | Self::Client(_)
            | Self::NativeLogin(_)
            | Self::Media(_) => fallback,
        }
    }
}

/// Native Waltr login progress with private presentation data redacted from Debug.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeLoginEvent {
    /// Startup credential restoration has settled.
    RestorationFinished,
    /// A device authorization is waiting for approval.
    AwaitingDevice(DeviceLoginPrompt),
    /// The provider does not support QR sign-in for this client.
    DeviceUnsupported,
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
    /// The exact attempt's installed credentials were accepted for reconnection.
    Succeeded {
        /// Reducer-issued generation admitted for completion.
        attempt_id: waltr_client::AccessAttemptId,
    },
    /// The flow failed with safe user-facing text.
    Failed {
        /// Safe failure text; never contains token response bodies.
        error: String,
    },
    /// The flow was canceled.
    Canceled {
        /// Reducer-issued generation whose cancellation won.
        attempt_id: waltr_client::AccessAttemptId,
    },
    /// In-memory credentials were removed.
    LoggedOut,
    /// Clipboard command completed.
    UrlCopyFinished {
        /// Whether a supported platform clipboard command succeeded.
        copied: bool,
    },
}

/// Short-lived device authorization shown only on the access screen.
#[derive(Clone, Eq, PartialEq)]
pub struct DeviceLoginPrompt {
    pub(crate) verification_uri: String,
    pub(crate) user_code: String,
    pub(crate) expires_at: Instant,
    pub(crate) qr: Option<QrCode>,
}

impl DeviceLoginPrompt {
    /// Prepare a QR from the complete URL when available, otherwise from the manual URL.
    #[must_use]
    pub fn new(
        verification_uri: String,
        verification_uri_complete: Option<&str>,
        user_code: String,
        expires_at: Instant,
    ) -> Self {
        let qr = QrCode::encode_text(
            verification_uri_complete.unwrap_or(&verification_uri),
            QrCodeEcc::Medium,
        )
        .ok();
        Self {
            verification_uri,
            user_code,
            expires_at,
            qr,
        }
    }
}

impl fmt::Debug for DeviceLoginPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeviceLoginPrompt([REDACTED])")
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
