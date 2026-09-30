//! Worker-side workspace tools and the private sandbox connection.
//!
//! The worker owns the filesystem root and execution tickets. Neither the host
//! nor a caller of the public tool API can execute without a prepared ticket.

mod patch;
#[cfg(feature = "shell")]
mod shell;
mod tools;
#[cfg(feature = "rpc")]
mod wire;

pub use tools::{Claim, Execution, Prepared, Sandbox, SandboxError, WorkspaceResult};
#[cfg(feature = "rpc")]
pub use wire::run_worker;
#[cfg(feature = "rpc")]
pub use wire::run_worker_with_shutdown;
