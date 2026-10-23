//! Resolved transport settings; no discovery or authentication.
use std::time::Duration;

use crate::error::Error;

/// Finite adapter limits. Defaults are adapter defaults, not product quotas.
#[derive(Clone, Debug)]
pub struct Config {
    /// Explicit `https://` (verified public roots) or deliberately chosen `http://` URI.
    pub uri: String,
    /// Connection establishment timeout (default five seconds).
    pub connect_timeout: Duration,
    /// Entire operation deadline, including stream finalization (default sixty seconds).
    pub request_timeout: Duration,
    /// Maximum encoded protobuf message bytes (default 4 MiB).
    pub max_encoding_bytes: usize,
    /// Maximum decoded protobuf message bytes (default 4 MiB).
    pub max_decoding_bytes: usize,
    /// Input and output channel capacity in messages (default eight each).
    pub queue_chunks: usize,
    /// Input queued protobuf bytes, excluding one currently polled caller item
    /// and the initial configuration (bounded by `max_encoding_bytes`).
    ///
    /// Default 1 MiB. Queued output protobuf bytes are bounded by
    /// `queue_chunks * max_decoding_bytes`, plus one in-flight decoded response.
    /// Transport flow-control buffers and caller-owned allocations are separate.
    pub queue_bytes: usize,
}
impl Config {
    /// Construct settings with conservative adapter defaults and an explicit URI.
    #[must_use]
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_mins(1),
            max_encoding_bytes: 4 * 1024 * 1024,
            max_decoding_bytes: 4 * 1024 * 1024,
            queue_chunks: 8,
            queue_bytes: 1024 * 1024,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let now = std::time::Instant::now();
        if now.checked_add(self.request_timeout).is_none()
            || now.checked_add(self.connect_timeout).is_none()
        {
            return Err(Error::Invalid("timeouts exceed clock range"));
        }
        if self.connect_timeout.is_zero()
            || self.request_timeout.is_zero()
            || self.max_encoding_bytes == 0
            || self.max_decoding_bytes == 0
            || self.queue_chunks == 0
            || self.queue_chunks > tokio::sync::Semaphore::MAX_PERMITS
            || self.queue_bytes == 0
            || self.queue_bytes > u32::MAX as usize
            || self.queue_bytes > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(Error::Invalid(
                "limits and timeouts must be positive and representable",
            ));
        }
        Ok(())
    }
}
