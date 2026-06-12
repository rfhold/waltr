//! LSP-backed tools and diagnostics.
//!
//! TODO: Add provider-facing LSP tools after the filesystem and session seams
//! can surface diagnostics and formatting results cleanly.

pub mod config;
pub mod diagnostics;
pub mod formatting;
pub mod hints;
pub mod tools;
