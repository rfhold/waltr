//! Native capture and PCM conversion, independent of UI frameworks and async runtimes.
//!
//! Device access occurs only through explicit `input_devices` and `Capture::start` calls.
//! Hosts own permission prompts, pause policy, and Android foreground policy. Android hosts
//! must obtain `RECORD_AUDIO` approval before capture. Android capture uses only the default
//! device with explicit 48 kHz mono I16 input and default buffering, avoiding CPAL's Java/context
//! discovery helpers. Named selection and enumeration are unsupported on Android. This known
//! context-independent path does not initialize ndk-context or retain Activity references.
//! Native open may fail: the requested input is not a hardware capability guarantee, and there
//! is no discovery or rate fallback. OS driver initialization/destruction remains unbounded.
//! Linux builds require ALSA development files/pkg-config; `PipeWire` uses ALSA compatibility.
//! Android NDK/C++ packaging must be verified by the composing application.
//!
//! The default wire format is 24 kHz mono S16LE. Enable the default `native` feature for
//! capture; disable it for pure conversion without native audio dependencies. CPAL uses 0.16.0
//! on macOS and 0.15.3 elsewhere (Oboe 0.6 / Android API 24); no shared-C++ runtime feature is
//! selected here.
//!
//! Run ownership operations on a host blocking worker, never a UI or audio callback thread.
//! Control is independently cloneable; the output reader needs no async runtime. For example:
//!
//! ```no_run
//! # #[cfg(feature = "native")]
//! # fn example() -> Result<(), waltr_audio_io::Error> {
//! use waltr_audio_io::{Capture, Config, Terminal};
//!
//! let (capture, output) = Capture::start(Config::default())?;
//! let control = capture.control(); // Move a clone to the host's event handler.
//! control.stop(); // A later host event normally requests this.
//! loop {
//!     while let Some(s16le) = output.try_frame() {
//!         // Deliver this bounded, frame-aligned chunk to the host's consumer.
//!         drop(s16le);
//!     }
//!     match output.terminal() {
//!         Terminal::Running => std::thread::sleep(std::time::Duration::from_millis(2)),
//!         Terminal::Completed => {
//!             // Completion can race the first drain: consume any last queued chunks.
//!             while let Some(s16le) = output.try_frame() { drop(s16le); }
//!             break;
//!         }
//!         Terminal::Cancelled | Terminal::Failed(_) => break,
//!     }
//! }
//! let terminal = capture.shutdown()?; // Cancels outstanding work and joins; never detaches.
//! # drop(terminal);
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod budget;
#[cfg(any(feature = "native", test))]
mod capture;
mod converter;
pub use budget::AudioBudget;
#[cfg(feature = "native")]
mod native;

#[cfg(any(feature = "native", test))]
pub use capture::{Capture, CaptureControl, CaptureOutput, Terminal};
pub use converter::{Converter, InputFormat, Samples};
#[cfg(feature = "native")]
pub use native::input_devices;

/// Device names must match uniquely; duplicate names are rejected rather than guessed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceSelection {
    #[default]
    /// The operating system default input device.
    Default,
    /// Exactly one device with this reported name; unsupported on Android.
    Name(String),
}

/// Target wire format: signed 16-bit little-endian PCM, without a WAV header.
#[derive(Clone, Debug)]
pub struct Config {
    /// Input device selection.
    pub device: DeviceSelection,
    /// Output sample rate in Hz (8000 through 48000).
    pub target_rate: u32,
    /// Only mono output is currently supported.
    pub target_channels: u16,
    /// Capture resource bounds.
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: DeviceSelection::Default,
            target_rate: 24_000,
            target_channels: 1,
            limits: Limits::default(),
        }
    }
}

/// Hard resource limits. Defaults can be lowered; duration may be raised to 3660 seconds
/// and `max_chunk_bytes` to 256 KiB. Raw buffers contain at most 4096 interleaved samples.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum accepted input duration, default 300, up to 3660 seconds.
    pub max_duration_seconds: u32,
    /// Upper bound on queued wire bytes, up to 1 MiB.
    pub output_queue_bytes: usize,
    /// Upper bound on queued chunks, up to 64.
    pub output_queue_items: usize,
    /// Even wire chunk bound, up to 256 KiB.
    pub max_chunk_bytes: usize,
    /// Raw callback queue capacity, one through eight.
    pub raw_queue_items: usize,
    /// Opt-in one-second native/transient audio reserve, including these external chunks.
    /// Include pending worker sends and consumer-owned frames (at most 64 chunks). Any separate
    /// host backlog must be bounded and added to this reserve by the caller, not counted twice.
    /// Native actual-format preflight rejects excess before pool allocation or stream construction.
    pub preaccept_external_chunks: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_duration_seconds: 300,
            output_queue_bytes: 1024 * 1024,
            output_queue_items: 64,
            max_chunk_bytes: 16 * 1024,
            raw_queue_items: 8,
            preaccept_external_chunks: None,
        }
    }
}

impl Config {
    /// Validate bounds before starting capture or allocating buffers.
    ///
    /// # Errors
    /// Returns `InvalidConfig` for unsupported bounds or Limit for arithmetic overflow.
    pub fn validate(&self) -> Result<(), Error> {
        let l = &self.limits;
        if !(8_000..=48_000).contains(&self.target_rate)
            || self.target_channels != 1
            || l.max_duration_seconds == 0
            || l.max_duration_seconds > 3660
            || l.output_queue_bytes < 2
            || l.output_queue_bytes > 1024 * 1024
            || l.output_queue_items == 0
            || l.output_queue_items > 64
            || l.max_chunk_bytes < 2
            || l.max_chunk_bytes > 256 * 1024
            || !l.max_chunk_bytes.is_multiple_of(2)
            || l.raw_queue_items == 0
            || l.raw_queue_items > 8
            || l.preaccept_external_chunks.is_some_and(|n| n > 64)
        {
            return Err(Error::InvalidConfig);
        }
        self.max_output_bytes()?;
        Ok(())
    }

    fn max_output_bytes(&self) -> Result<u64, Error> {
        u64::from(self.target_rate)
            .checked_mul(u64::from(self.limits.max_duration_seconds))
            .and_then(|n| n.checked_mul(2))
            .ok_or(Error::Limit)
    }
}

/// Errors contain no captured samples and implement no sample-bearing diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Configuration is outside supported bounds.
    InvalidConfig,
    /// Native rate, channel count, or sample representation is unsupported.
    UnsupportedFormat,
    /// This platform does not support device enumeration or named selection.
    UnsupportedDeviceSelection,
    /// The requested device does not exist.
    DeviceNotFound,
    /// Multiple devices share the requested name.
    AmbiguousDevice,
    /// Backend initialization, conversion, or streaming failed.
    Backend(String),
    /// Actual native layout exceeds the opt-in aggregate one-second audio capacity.
    PreacceptBudgetExceeded,
    /// The callback exhausted its bounded buffer budget.
    CallbackOverflow,
    /// No free callback block remained, including converter-owned blocks.
    CallbackPoolExhausted,
    /// The bounded raw queue could not accept an admitted callback block.
    CallbackQueueFull,
    /// An input or converted float is not finite.
    NonFiniteSample,
    /// Input does not contain complete interleaved frames.
    MisalignedInput,
    /// The cumulative duration or per-call resource limit was exceeded.
    Limit,
    /// The converter or capture was cancelled or closed.
    Closed,
    /// The owner thread panicked.
    WorkerPanicked,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod signal_tests;
#[cfg(test)]
mod tests;
