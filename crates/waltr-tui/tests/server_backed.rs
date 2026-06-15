//! TUI integration tests against a real Waltr server.

#![allow(clippy::future_not_send)]

mod support;

#[path = "server-backed/config.rs"]
mod config;

#[path = "server-backed/status.rs"]
mod status;
