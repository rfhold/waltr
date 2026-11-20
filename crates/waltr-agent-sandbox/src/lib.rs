//! Worker-side workspace tools and the private sandbox connection.
//!
//! The worker owns the filesystem root and execution tickets. Neither the host
//! nor a caller of the public tool API can execute without a prepared ticket.

#[allow(dead_code)] // The private worker tunnel will be the first production caller.
mod mcp_stdio;
mod patch;
#[cfg(feature = "rpc")]
pub mod peer_config;
#[cfg(feature = "shell")]
mod shell;
mod tools;
#[cfg(feature = "rpc")]
mod wire;

pub use tools::{Claim, Execution, Prepared, Sandbox, SandboxError, WorkspaceResult};
#[cfg(feature = "rpc")]
pub use wire::{run_worker, run_worker_with_authorization, run_worker_with_shutdown};
