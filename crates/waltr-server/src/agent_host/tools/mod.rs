//! Server-owned built-in capabilities.

pub mod filesystem;
pub mod question;
pub(crate) mod sandbox;
pub mod skills;
pub mod task;

#[cfg(feature = "shell")]
pub mod shell;
