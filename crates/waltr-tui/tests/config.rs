//! TUI config integration tests against a real Waltr server.

#![allow(clippy::future_not_send)]

mod support;

#[path = "config/read.rs"]
mod read;

#[path = "config/watch.rs"]
mod watch;

#[path = "config/providers.rs"]
mod providers;

#[path = "config/mcp.rs"]
mod mcp;
