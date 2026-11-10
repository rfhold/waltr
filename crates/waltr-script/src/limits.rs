use crate::Error;
use std::time::Duration;

/// Per-invocation ceilings. Every field must be nonzero and within the hard caps.
#[derive(Clone, Debug)]
pub struct Limits {
    /// UTF-8 source bytes (at most 64 KiB).
    pub source_bytes: usize,
    /// Encoded bytes of each structured input, argument, callback, or output.
    pub value_bytes: usize,
    /// Maximum structured depth.
    pub value_depth: usize,
    /// Maximum structured nodes (including object keys).
    pub value_nodes: usize,
    /// UTF-8 bytes per string or object key.
    pub string_bytes: usize,
    /// Bytes per binary payload.
    pub binary_bytes: usize,
    /// `QuickJS` allocator ceiling.
    pub heap_bytes: usize,
    /// `QuickJS` C-stack ceiling; Wasmi's interpreter stack is separately bounded.
    pub stack_bytes: usize,
    /// WASM linear memory ceiling.
    pub memory_bytes: usize,
    /// Fuel per indivisible engine operation; exhaustion terminates execution.
    pub fuel_slice: u64,
    /// Cumulative fuel; never replenished.
    pub fuel_total: u64,
    /// Host calls per invocation.
    pub host_calls: usize,
    /// Concurrent host futures and private bridge slots.
    pub in_flight: usize,
    /// Callback evaluations per invocation.
    pub callbacks: usize,
    /// Live resource identities.
    pub handles: usize,
    /// Execution and compile wall-clock deadline; at most 90 minutes, default 30 seconds.
    pub deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            source_bytes: 65_536,
            value_bytes: 65_536,
            value_depth: 32,
            value_nodes: 4096,
            string_bytes: 16_384,
            binary_bytes: 65_536,
            heap_bytes: 16 * 1024 * 1024,
            stack_bytes: 256 * 1024,
            memory_bytes: 32 * 1024 * 1024,
            fuel_slice: 20_000_000,
            fuel_total: 2_000_000_000,
            host_calls: 256,
            in_flight: 16,
            callbacks: 64,
            handles: 32,
            deadline: Duration::from_secs(30),
        }
    }
}

impl Limits {
    /// Reject zero, inconsistent, or unbounded limits before admission.
    ///
    /// # Errors
    /// Returns `Invalid` for zero, inconsistent, or out-of-range configuration.
    pub fn validate(&self) -> Result<(), Error> {
        for (value, cap) in [
            (self.source_bytes, 65_536),
            (self.value_bytes, 65_536),
            (self.value_depth, 64),
            (self.value_nodes, 8192),
            (self.string_bytes, 65_536),
            (self.binary_bytes, 65_536),
            (self.heap_bytes, 24 * 1024 * 1024),
            (self.stack_bytes, 512 * 1024),
            (self.memory_bytes, 32 * 1024 * 1024),
            (self.host_calls, 1024),
            (self.in_flight, 32),
            (self.callbacks, 1024),
            (self.handles, 256),
        ] {
            if value == 0 || value > cap {
                return Err(Error::Invalid);
            }
        }
        if self.memory_bytes < 4 * 1024 * 1024
            || self.heap_bytes < 1024 * 1024
            || self.stack_bytes < 16 * 1024
            || self.fuel_slice == 0
            || self.fuel_slice > 100_000_000
            || self.fuel_total < self.fuel_slice
            || self.fuel_total > 100_000_000_000
            || self.deadline.is_zero()
            || self.deadline > Duration::from_mins(90)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
