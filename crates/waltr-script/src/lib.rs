//! Invocation-owned JavaScript execution, without ambient host authority.
//!
//! See the crate README for the JavaScript ABI, ownership rules, and limits.
#![forbid(unsafe_code)]
#![allow(clippy::redundant_pub_crate)]

mod backend;
mod engine;
mod host;
mod limits;
mod value;

pub use engine::{
    Engine, EngineInfo, ExecutionReport, ScriptCancellation, ScriptEngine, ScriptExecution,
    ScriptFuture,
};
pub use host::{
    CallbackRef, CleanupFuture, HostContext, HostDispatcher, HostFuture, HostHandle, HostReply,
    HostRequest, InvocationId,
};
pub use limits::Limits;
pub use value::StructuredValue;

/// A bounded, trusted classification. Script text never determines this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Invalid configuration or value.
    Invalid,
    /// Source failed compile-only validation.
    Syntax,
    /// JavaScript threw or rejected a promise.
    Script,
    /// Structured value is unsupported or exceeds its budget.
    Value,
    /// A configured quota was exhausted.
    Limit,
    /// No global execution reservation is available.
    Busy,
    /// Cancellation was requested.
    Cancelled,
    /// The wall-clock deadline expired.
    Deadline,
    /// Interpreter instruction fuel was exhausted.
    Fuel,
    /// Guest allocation or linear memory failed.
    Memory,
    /// The host denied an operation.
    Denied,
    /// Invalid or expired callback/resource identity.
    Identity,
    /// An interpreter trap or private bridge invariant failed.
    Protocol,
    /// A worker panicked.
    Panic,
    /// Host cleanup did not verifiably complete.
    Cleanup,
    /// Engine shutdown or thread creation failure.
    Shutdown,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
