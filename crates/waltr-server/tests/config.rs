//! Server config integration tests.

mod support;

#[path = "config/read.rs"]
mod read;

#[path = "config/watch.rs"]
mod watch;

#[path = "config/providers.rs"]
mod providers;

#[path = "config/stale_etag.rs"]
mod stale_etag;

#[path = "config/reset.rs"]
mod reset;

#[path = "config/validation.rs"]
mod validation;

#[path = "config/mcp_server.rs"]
mod mcp_server;
