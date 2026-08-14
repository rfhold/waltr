//! TUI config integration tests against a real Waltr server.

#[path = "config/read.rs"]
mod read;

#[path = "config/watch.rs"]
mod watch;

#[path = "config/providers.rs"]
mod providers;

#[path = "config/mcp.rs"]
mod mcp;

#[path = "config/inference_defaults.rs"]
mod inference_defaults;

#[path = "config/reset.rs"]
mod reset;
