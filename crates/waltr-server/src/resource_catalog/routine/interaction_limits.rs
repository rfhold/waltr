//! Server-owned limits contract for future interaction admission and execution.

use serde::{Deserialize, Serialize};

/// Configurable resource bounds for interaction admission and execution.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct InteractionLimits {
    /// Maximum bytes in one complete interaction request.
    pub complete_request_bytes: usize,
    /// Maximum bytes in one audio or text chunk.
    pub chunk_bytes: usize,
    /// Maximum buffered bytes per stream edge.
    pub edge_bytes: usize,
    /// Maximum queued chunks per stream edge.
    pub queued_chunks: usize,
    /// Maximum concurrently admitted interactions.
    pub active_interactions: usize,
    /// Maximum active tasks within one interaction.
    pub active_tasks_per_interaction: usize,
    /// Maximum wait for byte and item capacity on each bounded queue enqueue.
    pub queue_wait_seconds: u64,
    /// Maximum input duration in seconds.
    pub input_seconds: u64,
    /// Maximum total invocation duration in seconds.
    pub invocation_seconds: u64,
    /// Maximum collected text bytes before task and model context limits.
    pub collector_bytes: usize,
}

impl Default for InteractionLimits {
    fn default() -> Self {
        Self {
            complete_request_bytes: 32 * 1024 * 1024,
            chunk_bytes: 256 * 1024,
            edge_bytes: 1024 * 1024,
            queued_chunks: 8,
            active_interactions: 16,
            active_tasks_per_interaction: 8,
            queue_wait_seconds: 30,
            input_seconds: 15 * 60,
            invocation_seconds: 20 * 60,
            collector_bytes: 1024 * 1024,
        }
    }
}

impl InteractionLimits {
    /// Validate before constructing bounded channels or admitting interactions.
    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            self.complete_request_bytes,
            self.chunk_bytes,
            self.edge_bytes,
            self.queued_chunks,
            self.active_interactions,
            self.active_tasks_per_interaction,
            self.collector_bytes,
        ]
        .contains(&0)
            || self.queue_wait_seconds == 0
            || self.input_seconds == 0
            || self.invocation_seconds == 0
        {
            return Err("interaction limits must be positive");
        }
        if self.chunk_bytes > self.complete_request_bytes
            || self.chunk_bytes > self.edge_bytes
            || self.input_seconds > self.invocation_seconds
        {
            return Err("interaction chunk and duration limits are inconsistent");
        }
        if std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(self.queue_wait_seconds))
            .is_none()
        {
            return Err("interaction queue wait duration is not representable");
        }
        if self.chunk_bytes.checked_mul(self.queued_chunks).is_none() {
            return Err("interaction queue byte capacity overflows");
        }
        Ok(())
    }
}
