//! TUI status integration tests against a real Waltr server.

#![allow(clippy::future_not_send)]

mod support;

#[path = "status/landing.rs"]
mod landing;

#[path = "status/startup.rs"]
mod startup;
